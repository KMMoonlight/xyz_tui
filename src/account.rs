use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use tokio::sync::{Mutex, oneshot};

use crate::{
    auth::{self, Credentials, Error},
    content,
};

pub struct Renewal {
    pub epoch: u64,
    pub credentials: Credentials,
    pub saved: oneshot::Sender<bool>,
}

/// Authenticated operations share one credential owner and one refresh at a time.
pub struct Account {
    credentials: Mutex<Credentials>,
    auth: auth::Api,
    content: content::Api,
    epoch: u64,
    cancelled: AtomicBool,
    pending: AtomicUsize,
    renew: Arc<dyn Fn(Renewal) + Send + Sync>,
}

impl Account {
    pub fn new(
        credentials: Credentials,
        auth: auth::Api,
        content: content::Api,
        epoch: u64,
        renew: Arc<dyn Fn(Renewal) + Send + Sync>,
    ) -> Self {
        Self {
            credentials: Mutex::new(credentials),
            auth,
            content,
            epoch,
            cancelled: AtomicBool::new(false),
            pending: AtomicUsize::new(0),
            renew,
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_idle(&self) -> bool {
        self.pending.load(Ordering::SeqCst) == 0
    }

    pub async fn request<T: Send + 'static, F, Fut>(
        self: &Arc<Self>,
        operation: F,
    ) -> Result<T, Error>
    where
        F: Fn(content::Api, Credentials) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T, Error>> + Send,
    {
        // Changing selection must not cancel a token rotation after the server accepts it.
        self.pending.fetch_add(1, Ordering::SeqCst);
        let pending = Pending(self.clone());
        tokio::spawn(async move { pending.0.perform(operation).await })
            .await
            .map_err(|_| Error::Network)?
    }

    async fn perform<T: Send, F, Fut>(&self, operation: F) -> Result<T, Error>
    where
        F: Fn(content::Api, Credentials) -> Fut + Send,
        Fut: std::future::Future<Output = Result<T, Error>> + Send,
    {
        let mut credentials = self.credentials.lock().await;
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(Error::Http(reqwest::StatusCode::UNAUTHORIZED));
        }
        let result = operation(self.content.clone(), credentials.clone()).await;
        if !matches!(result, Err(Error::Http(reqwest::StatusCode::UNAUTHORIZED))) {
            return result;
        }
        if self.cancelled.load(Ordering::SeqCst) {
            return result;
        }
        let renewed = self.auth.refresh(&credentials).await?;
        let (saved, receiver) = oneshot::channel();
        (self.renew)(Renewal {
            epoch: self.epoch,
            credentials: renewed.clone(),
            saved,
        });
        if !receiver.await.unwrap_or(false) || self.cancelled.load(Ordering::SeqCst) {
            return Err(Error::Http(reqwest::StatusCode::UNAUTHORIZED));
        }
        *credentials = renewed;
        operation(self.content.clone(), credentials.clone()).await
    }
}

struct Pending(Arc<Account>);

impl Drop for Pending {
    fn drop(&mut self) {
        self.0.pending.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;
    use tokio::sync::mpsc;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, path},
    };

    #[tokio::test]
    async fn cancelled_caller_does_not_lose_rotation_and_concurrent_request_reuses_saved_tokens() {
        let server = MockServer::start().await;
        Mock::given(path("/v1/playlist/pull"))
            .and(header("x-jike-access-token", "old"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/app_auth_tokens.refresh"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-jike-access-token", "new")
                    .insert_header("x-jike-refresh-token", "rotated"),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/v1/playlist/pull"))
            .and(header("x-jike-access-token", "new"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":{"list":[]}})))
            .expect(2)
            .mount(&server)
            .await;
        let (sender, mut renewals) = mpsc::unbounded_channel();
        let account = Arc::new(Account::new(
            Credentials {
                access_token: "old".into(),
                refresh_token: "refresh".into(),
            },
            auth::Api::for_test(server.uri()),
            content::Api::for_test(server.uri()),
            4,
            Arc::new(move |renewal| {
                sender.send(renewal).ok();
            }),
        ));
        let first = account.clone();
        let caller = tokio::spawn(async move {
            first
                .request(|api, c| async move { api.playlist(&c).await })
                .await
        });
        let renewal = tokio::time::timeout(Duration::from_secs(2), renewals.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(renewal.epoch, 4);
        assert_eq!(renewal.credentials.refresh_token, "rotated");
        caller.abort();
        let second = account.clone();
        let next = tokio::spawn(async move {
            second
                .request(|api, c| async move { api.playlist(&c).await })
                .await
        });
        tokio::task::yield_now().await;
        assert!(!account.is_idle());
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "no request may use unsaved rotated credentials"
        );
        renewal.saved.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), next)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(account.is_idle());
    }
}
