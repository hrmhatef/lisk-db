use thiserror::Error;

const MAX_VARINT_LEN: usize = 10;

#[derive(Error, Debug)]
pub enum CodecError {
    #[error("Invalid bytes length")]
    InvalidBytesLength,
    #[error("Out of range")]
    OutOfRange,
    #[error("No termination code")]
    NoTermination,
    #[error("Invalid wire type")]
    InvalidWireType,
}

pub struct Reader<'a> {
    index: usize,
    end: usize,
    data: &'a [u8],
}

pub struct Writer {
    result: Vec<u8>,
    size: usize,
}

fn write_varint(value: u32) -> Vec<u8> {
    let mut value = value;
    let mut result = vec![0; MAX_VARINT_LEN];
    let mut index = 0;
    while value > 0x7f {
        result[index] = 0x80 | (value & 0x7f) as u8;
        value >>= 7;
        index += 1;
    }
    result[index] = value as u8;

    result[0..index + 1].to_vec()
}

fn read_varint(data: &[u8], offset: usize) -> Result<(u32, usize), CodecError> {
    let mut result: u32 = 0;
    let mut index = offset;
    let mut shift = 0;
    while shift < 32 {
        if index >= data.len() {
            return Err(CodecError::InvalidBytesLength);
        }
        let bit = data[index] as u32;
        index += 1;
        if index == offset + 5 && bit > 0x0f {
            return Err(CodecError::OutOfRange);
        }
        result |= (bit & 0x7f_u32) << shift;
        if (bit & 0x80) == 0 {
            return Ok((result, index - offset));
        }

        shift += 7;
    }
    Err(CodecError::NoTermination)
}

fn read_key(val: u32) -> Result<(u32, u32), CodecError> {
    let wire_type = val & 7;
    if wire_type != 0 && wire_type != 2 {
        return Err(CodecError::InvalidWireType);
    }
    let field_number = val >> 3;
    Ok((field_number, wire_type))
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        let length = data.len();
        Self {
            data,
            index: 0,
            end: length,
        }
    }

    pub fn read_bytes_slice(&mut self, field_number: u32) -> Result<Vec<Vec<u8>>, CodecError> {
        let mut result = vec![];
        while self.index < self.end {
            let ok = self.check(field_number)?;
            if !ok {
                return Ok(result);
            }
            let value = self.read_only_bytes()?;
            result.push(value);
        }

        Ok(result)
    }

    pub fn read_bytes(&mut self, field_number: u32) -> Result<Vec<u8>, CodecError> {
        let ok = self.check(field_number)?;
        match ok {
            true => self.read_only_bytes(),
            false => Ok(vec![]),
        }
    }

    fn read_only_bytes(&mut self) -> Result<Vec<u8>, CodecError> {
        let (result, size) = read_varint(self.data, self.index)?;
        self.index += size;
        if result as usize > self.data.len() {
            return Err(CodecError::InvalidBytesLength);
        }
        let decoded = self.data[self.index..self.index + result as usize].to_vec();
        self.index += result as usize;

        Ok(decoded)
    }

    fn check(&mut self, field_number: u32) -> Result<bool, CodecError> {
        if self.index >= self.end {
            return Ok(false);
        }

        let (key, size) = read_varint(self.data, self.index)?;
        let (next_field_number, _) = read_key(key)?;
        if field_number != next_field_number {
            return Ok(false);
        }
        self.index += size;
        Ok(true)
    }
}

impl Writer {
    pub fn new() -> Self {
        Self {
            result: vec![],
            size: 0,
        }
    }

    pub fn write_bytes(&mut self, field_number: u32, value: &[u8]) {
        self.write_key(2, field_number);
        self.write_varint(value.len() as u32);
        self.size += value.len();
        self.result.extend(value);
    }

    pub fn write_bytes_slice(&mut self, field_number: u32, values: &[Vec<u8>]) {
        if values.is_empty() {
            return;
        }
        for val in values.iter() {
            self.write_bytes(field_number, val);
        }
    }

    pub fn result(&self) -> Vec<u8> {
        self.result.clone()
    }

    fn write_key(&mut self, wire_type: u32, field_number: u32) {
        let key = (field_number << 3) | wire_type;
        let key_bytes = write_varint(key);
        self.size += key_bytes.len();
        self.result.extend(key_bytes);
    }

    fn write_varint(&mut self, val: u32) {
        let val_bytes = write_varint(val);
        self.size += val_bytes.len();
        self.result.extend(val_bytes);
    }
}
