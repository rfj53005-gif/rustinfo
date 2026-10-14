//! CPU identity shared by monitoring and the hardware control guard.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CpuIdentity {
    pub vendor: String,
    pub family: Option<u32>,
    pub model: Option<u32>,
    pub brand: String,
}

impl CpuIdentity {
    pub fn parse(text: &str) -> Self {
        let mut cpu = Self::default();
        for line in text.lines().take_while(|line| !line.trim().is_empty()) {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "vendor_id" => cpu.vendor = value.into(),
                "cpu family" => cpu.family = value.parse().ok(),
                "model" => cpu.model = value.parse().ok(),
                "model name" => cpu.brand = value.into(),
                _ => {}
            }
        }
        cpu
    }

    pub fn supports_smu(&self) -> bool {
        self.vendor == "AuthenticAMD" && self.family == Some(26) && self.model == Some(36)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_shared_family_is_not_enough_to_allow_mailbox_access() {
        for (vendor, family, model, supported) in [
            ("AuthenticAMD", 26, 36, true),
            ("AuthenticAMD", 26, 68, false),
            ("AuthenticAMD", 26, 112, false),
            ("GenuineIntel", 26, 36, false),
            ("AuthenticAMD", 25, 36, false),
        ] {
            let cpu = CpuIdentity::parse(&format!("vendor_id : {vendor}\ncpu family : {family}\nmodel : {model}\nmodel name : Example"));
            assert_eq!(cpu.supports_smu(), supported);
        }
        assert!(!CpuIdentity::parse("model name: AMD\n").supports_smu());
    }
}
