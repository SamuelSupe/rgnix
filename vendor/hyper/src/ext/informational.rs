use std::sync::Arc;

#[derive(Clone)]
pub(crate) enum OnInformational {
    Callback(Arc<dyn OnInformationalCallback + Send + Sync>),
    #[cfg(all(feature = "server", feature = "rgnix-extensions"))]
    Forward(super::SendInformational),
}

/// Add a callback for 1xx informational responses.
///
/// # Example
///
/// ```
/// # let some_body = ();
/// let mut req = hyper::Request::new(some_body);
///
/// hyper::ext::on_informational(&mut req, |res| {
///     println!("informational: {:?}", res.status());
/// });
///
/// // send request on a client connection...
/// ```
pub fn on_informational<B, F>(req: &mut http::Request<B>, callback: F)
where
    F: Fn(Response<'_>) + Send + Sync + 'static,
{
    on_informational_raw(req, OnInformationalClosure(callback));
}

pub(crate) fn on_informational_raw<B, C>(req: &mut http::Request<B>, callback: C)
where
    C: OnInformationalCallback + Send + Sync + 'static,
{
    req.extensions_mut()
        .insert(OnInformational::Callback(Arc::new(callback)));
}

// Sealed, not actually nameable bounds
pub(crate) trait OnInformationalCallback {
    fn on_informational(&self, res: http::Response<()>);
}

impl OnInformational {
    pub(crate) fn take(extensions: &mut http::Extensions) -> Option<Self> {
        let callback = extensions.remove::<Self>();
        #[cfg(all(feature = "server", feature = "rgnix-extensions"))]
        if callback.is_none()
            && extensions
                .get::<super::SendInformational>()
                .is_some_and(|sender| sender.forward.is_some())
        {
            let sender = extensions.remove::<super::SendInformational>()?;
            sender.reuse_extensions(extensions);
            return Some(Self::Forward(sender));
        }
        callback
    }

    pub(crate) fn call(&self, res: http::Response<()>) {
        match self {
            Self::Callback(callback) => callback.on_informational(res),
            #[cfg(all(feature = "server", feature = "rgnix-extensions"))]
            Self::Forward(sender) => {
                let mut res = res;
                (sender.forward.expect("forwarding sender"))(&mut res);
                let _ = sender.try_send(res);
            }
        }
    }
}

struct OnInformationalClosure<F>(F);

impl<F> OnInformationalCallback for OnInformationalClosure<F>
where
    F: Fn(Response<'_>) + Send + Sync + 'static,
{
    fn on_informational(&self, res: http::Response<()>) {
        let res = Response(&res);
        (self.0)(res);
    }
}

// A facade over http::Response.
//
// It purposefully hides being able to move the response out of the closure,
// while also not being able to expect it to be a reference `&Response`.
// (Otherwise, a closure can be written as `|res: &_|`, and then be broken if
// we make the closure take ownership.)
//
// With the type not being nameable, we could change from being a facade to
// being either a real reference, or moving the http::Response into the closure,
// in a backwards-compatible change in the future.
#[derive(Debug)]
pub struct Response<'a>(&'a http::Response<()>);

impl Response<'_> {
    #[inline]
    pub fn status(&self) -> http::StatusCode {
        self.0.status()
    }

    #[inline]
    pub fn version(&self) -> http::Version {
        self.0.version()
    }

    #[inline]
    pub fn headers(&self) -> &http::HeaderMap {
        self.0.headers()
    }
}

#[cfg(all(test, feature = "server", feature = "rgnix-extensions"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn forwarded_hints_keep_filtering_and_request_isolation() {
        let (sender, mut receiver) = super::super::SendInformational::channel();
        let mut extensions = http::Extensions::new();
        extensions.insert(sender);
        extensions.insert(7usize);
        assert!(OnInformational::take(&mut extensions).is_none());
        extensions
            .get_mut::<super::super::SendInformational>()
            .unwrap()
            .forward(|response| {
                response.headers_mut().remove("x-hop");
            });
        let callback = OnInformational::take(&mut extensions).unwrap();
        assert_eq!(extensions.get::<usize>(), Some(&7));
        let hint = || {
            http::Response::builder()
                .status(103)
                .header("x-hop", "hidden")
                .body(())
                .unwrap()
        };
        callback.call(hint());
        let response = std::future::poll_fn(|cx| receiver.poll_recv(cx))
            .await
            .unwrap();
        assert_eq!(response.status(), 103);
        assert!(!response.headers().contains_key("x-hop"));
        receiver.end_request();
        let current = receiver.begin_request();
        let mut next = http::Extensions::new();
        current.reuse_extensions(&mut next);
        assert!(next.get::<usize>().is_none());
        callback.call(hint());
        current.try_send(hint()).unwrap();
        let response = std::future::poll_fn(|cx| receiver.poll_recv(cx))
            .await
            .unwrap();
        assert_eq!(response.headers()["x-hop"], "hidden");
        drop(current);
        assert!(std::future::poll_fn(|cx| receiver.poll_recv(cx))
            .await
            .is_none());
    }
}
