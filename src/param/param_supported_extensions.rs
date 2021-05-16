use super::{param_header::*, param_type::*, *};
use crate::chunk::chunk_type::*;

use bytes::{Buf, BufMut, Bytes, BytesMut};

#[derive(Default, Debug, Clone, PartialEq)]
pub(crate) struct ParamSupportedExtensions {
    pub(crate) chunk_types: Vec<ChunkType>,
}

impl fmt::Display for ParamSupportedExtensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}",
            self.header(),
            self.chunk_types
                .iter()
                .map(|ct| ct.to_string())
                .collect::<Vec<String>>()
                .join(" "),
        )
    }
}

impl Param for ParamSupportedExtensions {
    fn header(&self) -> ParamHeader {
        ParamHeader {
            typ: ParamType::SupportedExt,
            value_length: self.value_length() as u16,
        }
    }

    fn unmarshal(raw: &Bytes) -> Result<Self, Error> {
        let header = ParamHeader::unmarshal(raw)?;

        let reader =
            &mut raw.slice(PARAM_HEADER_LENGTH..PARAM_HEADER_LENGTH + header.value_length());

        let mut chunk_types = vec![];
        while reader.has_remaining() {
            chunk_types.push(ChunkType(reader.get_u8()));
        }

        Ok(ParamSupportedExtensions { chunk_types })
    }

    fn marshal_to(&self, buf: &mut BytesMut) -> Result<usize, Error> {
        self.header().marshal_to(buf)?;
        for ct in &self.chunk_types {
            buf.put_u8(ct.0);
        }
        Ok(buf.len())
    }

    fn value_length(&self) -> usize {
        self.chunk_types.len()
    }

    fn clone_to(&self) -> Box<dyn Param> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
