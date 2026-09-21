use agl_core::agent::PackageDigest;
use serde::{Deserialize, Serialize};

macro_rules! digest {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name([u8; 32]);

        impl $name {
            pub const fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }

            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }

            pub fn parse(value: impl AsRef<str>) -> Result<Self, &'static str> {
                let hex = value
                    .as_ref()
                    .strip_prefix("sha256:")
                    .ok_or("digest requires sha256 prefix")?;
                if hex.len() != 64
                    || !hex
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                {
                    return Err("digest requires 64 lowercase hexadecimal characters");
                }
                let mut bytes = [0; 32];
                for (index, pair) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
                    bytes[index] = u8::from_str_radix(
                        std::str::from_utf8(pair).expect("ASCII digest was validated"),
                        16,
                    )
                    .map_err(|_| "invalid digest")?;
                }
                Ok(Self(bytes))
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("sha256:")?;
                for byte in self.0 {
                    write!(formatter, "{byte:02x}")?;
                }
                Ok(())
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}

digest!(PhysicalDeviceDigest);
digest!(DriverBuildDigest);
digest!(EngineBuildDigest);
digest!(RuntimeProfileDigest);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceFailureKind {
    EngineCrash,
    UnattributedSignal { signal: i32 },
    DeviceLost,
    InvalidAllocation,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerHealth {
    pub physical_device: PhysicalDeviceDigest,
    pub driver_build: DriverBuildDigest,
    pub engine_build: EngineBuildDigest,
    pub crash_streak: u32,
    pub retry_after_ms: i64,
    pub last_failure_kind: InferenceFailureKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceQuarantine {
    pub physical_device: PhysicalDeviceDigest,
    pub driver_build: DriverBuildDigest,
    pub engine_build: EngineBuildDigest,
    pub model: PackageDigest,
    pub runtime_profile: RuntimeProfileDigest,
    pub admitted_host_bytes: u64,
    pub observed_host_bytes: u64,
    pub admitted_device_bytes: u64,
    pub observed_device_bytes: u64,
    pub admitted_shared_bytes: u64,
    pub observed_shared_bytes: u64,
    pub recorded_at_ms: i64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RestoredInferenceHealth {
    pub workers: Vec<WorkerHealth>,
    pub quarantines: Vec<ResourceQuarantine>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InferenceHealthUpdate {
    Worker(WorkerHealth),
    Quarantine(ResourceQuarantine),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_digests_are_exact_and_canonical() {
        assert!(PhysicalDeviceDigest::parse(format!("sha256:{}", "a".repeat(64))).is_ok());
        assert!(PhysicalDeviceDigest::parse(format!("sha256:{}", "A".repeat(64))).is_err());
        assert!(PhysicalDeviceDigest::parse("gpu0").is_err());
        let digest = PhysicalDeviceDigest::from_bytes([0xab; 32]);
        assert_eq!(digest.as_bytes(), &[0xab; 32]);
        assert_eq!(digest.to_string(), format!("sha256:{}", "ab".repeat(32)));
    }
}
