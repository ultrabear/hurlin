use rand::Rng;

#[derive(Eq, Hash, PartialEq, Copy, Clone)]
pub struct HexKey([u8; 16]);

impl HexKey {
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.0).unwrap()
    }

    pub fn validate(data: &str) -> Result<Self, ()> {
        let Ok(bytes) = <[u8; 16]>::try_from(data.as_bytes()) else {
            return Err(());
        };

        if bytes.iter().all(|b| b.is_ascii_hexdigit()) {
            return Ok(Self(bytes));
        } else {
            return Err(());
        }
    }

    /// generates a new random taskid
    pub fn new() -> Self {
        let data: [u8; 8] = rand::thread_rng().gen();

        Self(hex::encode(&data).as_bytes().try_into().unwrap())
    }
}

macro_rules! basically_hexkey {
    ($type:ident) => {
        #[derive(Eq, Hash, PartialEq, Copy, Clone)]
        pub struct $type(crate::hexkey::HexKey);

        impl $type {
            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }

            pub fn validate(data: &str) -> Result<Self, ()> {
                crate::hexkey::HexKey::validate(data).map(Self)
            }

            pub fn new() -> Self {
                Self(crate::hexkey::HexKey::new())
            }
        }

        impl fmt::Display for $type {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.as_str())
            }
        }
    };
}

pub(crate) use basically_hexkey;
