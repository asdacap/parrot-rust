use super::{CookieStore, CookieStoreFactory};
use std::sync::{Arc, Mutex};
use url::Url;

#[derive(Clone, Copy, Debug, Default)]
pub struct FetchCookieStoreFactory;

impl CookieStoreFactory for FetchCookieStoreFactory {
    fn create(&self) -> Arc<dyn CookieStore> {
        Arc::new(FetchCookies(Mutex::new(
            cookie_store::CookieStore::default(),
        )))
    }
}

struct FetchCookies(Mutex<cookie_store::CookieStore>);

impl CookieStore for FetchCookies {
    fn request_header(&self, address: &Url) -> Option<String> {
        let store = self.0.lock().expect("fetch cookie store lock poisoned");
        let values = store
            .get_request_values(address)
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>();
        (!values.is_empty()).then(|| values.join("; "))
    }

    fn store_response(&self, address: &Url, set_cookie: &[String]) {
        let mut store = self.0.lock().expect("fetch cookie store lock poisoned");
        for header in set_cookie {
            let _ = store.parse(header, address);
        }
    }
}
