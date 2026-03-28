use crate::error::{RecurramError, Result};

pub fn encode_varuint(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

pub fn encode_zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

pub fn decode_zigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

pub fn encode_bytes(bytes: &[u8], out: &mut Vec<u8>) {
    encode_varuint(bytes.len() as u64, out);
    out.extend_from_slice(bytes);
}

pub fn encode_string(value: &str, out: &mut Vec<u8>) {
    encode_bytes(value.as_bytes(), out);
}

pub fn encode_bitmap(bits: &[bool], out: &mut Vec<u8>) {
    encode_varuint(bits.len() as u64, out);
    let mut current = 0u8;
    for (i, bit) in bits.iter().enumerate() {
        if *bit {
            current |= 1 << (i % 8);
        }
        if i % 8 == 7 {
            out.push(current);
            current = 0;
        }
    }
    if !bits.len().is_multiple_of(8) {
        out.push(current);
    }
}

pub struct Reader<'a> {
    input: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    pub fn new(input: &'a [u8]) -> Self {
        Self { input, offset: 0 }
    }

    pub fn position(&self) -> usize {
        self.offset
    }

    pub fn is_eof(&self) -> bool {
        self.offset >= self.input.len()
    }

    pub fn read_u8(&mut self) -> Result<u8> {
        let byte = *self
            .input
            .get(self.offset)
            .ok_or(RecurramError::UnexpectedEof)?;
        self.offset += 1;
        Ok(byte)
    }

    pub fn read_exact(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(RecurramError::InvalidData("offset overflow"))?;
        let slice = self
            .input
            .get(self.offset..end)
            .ok_or(RecurramError::UnexpectedEof)?;
        self.offset = end;
        Ok(slice)
    }

    pub fn read_varuint(&mut self) -> Result<u64> {
        let mut shift = 0u32;
        let mut result = 0u64;
        loop {
            if shift >= 64 {
                return Err(RecurramError::InvalidData("varuint too large"));
            }
            let byte = self.read_u8()?;
            result |= ((byte & 0x7F) as u64) << shift;
            if (byte & 0x80) == 0 {
                return Ok(result);
            }
            shift += 7;
        }
    }

    pub fn read_i64_zigzag(&mut self) -> Result<i64> {
        let encoded = self.read_varuint()?;
        Ok(decode_zigzag(encoded))
    }

    pub fn read_bytes(&mut self) -> Result<Vec<u8>> {
        let len = self.read_varuint()? as usize;
        Ok(self.read_exact(len)?.to_vec())
    }

    pub fn read_string(&mut self) -> Result<String> {
        let bytes = self.read_bytes()?;
        String::from_utf8(bytes).map_err(|_| RecurramError::Utf8Error)
    }

    pub fn read_bitmap(&mut self) -> Result<Vec<bool>> {
        let bit_count = self.read_varuint()? as usize;
        let byte_count = bit_count.div_ceil(8);
        let bytes = self.read_exact(byte_count)?;
        let mut bits = Vec::with_capacity(bit_count);
        for i in 0..bit_count {
            let byte = bytes[i / 8];
            bits.push(((byte >> (i % 8)) & 1) == 1);
        }
        Ok(bits)
    }
}
