pub use self::body::Body;
pub use self::client::{
    Client, ClientBuilder, ClientUpdate, apply_http1_config2, apply_http2_config2,
};
pub use self::config::{Http1Config, Http2Config};
pub use self::emulation::{EmulationProvider, EmulationProviderFactory};
pub use self::request::{Request, RequestBuilder};
pub use self::response::Response;
pub use self::upgrade::Upgraded;

pub mod body;
#[allow(clippy::module_inception)]
mod client;
mod config;
pub mod decoder;
mod emulation;
#[cfg(feature = "multipart")]
pub mod multipart;
pub(crate) mod request;
mod response;
mod upgrade;
#[cfg(feature = "websocket")]
pub mod websocket;
