use super::{param_header::*, param_type::*, *};

use bytes::{Bytes, BytesMut};
use rand::Rng;
use std::fmt;

#[derive(Debug, Clone)]
pub(crate) struct ParamStateCookie {
    pub(crate) cookie: Bytes,
}

/// String makes paramStateCookie printable
impl fmt::Display for ParamStateCookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {:?}", self.header(), self.cookie)
    }
}

impl Param for ParamStateCookie {
    fn unmarshal(raw: &Bytes) -> Result<Self, Error> {
        let _ = ParamHeader::unmarshal(raw)?;
        let cookie = raw.slice(PARAM_HEADER_LENGTH..);
        Ok(ParamStateCookie { cookie })
    }

    fn marshal_to(&self, buf: &mut BytesMut) -> Result<usize, Error> {
        self.header().marshal_to(buf)?;
        buf.extend(self.cookie.clone());
        Ok(buf.len())
    }

    fn value_length(&self) -> usize {
        self.cookie.len()
    }
}

impl ParamStateCookie {
    pub(crate) fn header(&self) -> ParamHeader {
        ParamHeader {
            typ: ParamType::StateCookie,
            value_length: self.value_length() as u16,
        }
    }

    pub(crate) fn new() -> Self {
        let mut cookie = BytesMut::new();
        cookie.resize(32, 0);
        rand::thread_rng().fill(cookie.as_mut());

        ParamStateCookie {
            cookie: cookie.freeze(),
        }
    }
}