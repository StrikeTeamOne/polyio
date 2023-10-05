use crate::api::ticker::Ticker;
use serde::Deserialize;

use crate::api::response::Response;
use crate::Str;

/// All tickers as returned by the `/v2/reference/tickers/`
/// endpoint.
///
/// Please note that not all fields available in a request are
/// represented here.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct TickersResp {
    /// Vector of ticker information.
    #[serde(rename = "ticker")]
    pub tickers: Vec<Ticker>,
}

Endpoint! {
  /// The representation of a GET request to the
  /// `/v2/reference/tickers/` endpoint.
  pub Get(()),
  Ok => Response<TickersResp>, [
    /// The ticker information was retrieved successfully.
    /* 200 */ OK,
  ],
  Err => GetError, [
    /// The specified resource was not found.
    ///
    /// This error will also occur on valid tickers when the market is
    /// closed.
    /* 404 */ NOT_FOUND => NotFound,
  ]

  fn path(_input: &Self::Input) -> Str {
    "/v2/reference/tickers/".to_string().into()
  }
}
