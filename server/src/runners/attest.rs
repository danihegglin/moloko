//! Attestation: how the customer's moloko server decides that a cloud runner is a genuine
//! confidential VM running an image it accepts, before it sends anything.
//!
//! SEV-SNP evidence is the 1184-byte attestation report the AMD secure processor signs, plus the
//! chip's VCEK certificate. We check VCEK <- ASK <- ARK against AMD's root certificates built into
//! this binary, the report signature, the guest policy, the launch measurement against the
//! customer's allow-list, and that the report's user data binds the runner's channel key.

use anyhow::{Context, Result, anyhow, bail, ensure};
use rsa::signature::Verifier;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use x509_cert::der::{Decode, DecodePem, Encode};

pub const REPORT_LEN: usize = 1184;
const SIGNED_LEN: usize = 0x2A0;
const POLICY_MIGRATE_MA: u64 = 1 << 18;
const POLICY_DEBUG: u64 = 1 << 19;

/// AMD's ASK + ARK per CPU generation, as served by kdsintf.amd.com/vcek/v1/<gen>/cert_chain.
const AMD_CHAINS: &[(&str, &str)] = &[
    ("Milan", include_str!("certs/milan.pem")),
    ("Genoa", include_str!("certs/genoa.pem")),
    ("Turin", include_str!("certs/turin.pem")),
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Evidence {
    /// A real SEV-SNP report (hex) and the chip's VCEK certificate (DER, hex).
    SevSnp { report: String, vcek: String },
    /// No hardware: a local runner for development. Accepted only with `--cloud-insecure-dev`.
    Dev,
}

/// What the customer's server accepts.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    /// Launch measurements (96 hex chars) of runner images this server trusts.
    pub measurements: Vec<String>,
    /// Accept runners without hardware attestation. Never in production.
    pub insecure_dev: bool,
}

/// What a passed check proved, shown on the job in the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Verified {
    pub kind: String,
    pub measurement: String,
    pub chip: String,
    pub reported_tcb: String,
}

/// The 64 bytes a runner asks the CPU to sign: they tie the report to this lease and to the
/// key pair both sides use for the channel, so a relay can't swap in its own key.
pub fn binding(runner_pub: &[u8; 32], server_pub: &[u8; 32], nonce: &[u8; 32]) -> [u8; 64] {
    let mut h = Sha512::new();
    h.update(b"moloko-runner-v1");
    h.update(runner_pub);
    h.update(server_pub);
    h.update(nonce);
    h.finalize().into()
}

pub fn verify(policy: &Policy, ev: &Evidence, expect: &[u8; 64]) -> Result<Verified> {
    match ev {
        Evidence::Dev => {
            ensure!(
                policy.insecure_dev,
                "runner offered no hardware attestation (dev mode), and --cloud-insecure-dev is off"
            );
            Ok(Verified {
                kind: "dev (insecure)".into(),
                measurement: String::new(),
                chip: String::new(),
                reported_tcb: String::new(),
            })
        }
        Evidence::SevSnp { report, vcek } => {
            let report = hex::decode(report).context("report is not hex")?;
            let vcek = hex::decode(vcek).context("vcek is not hex")?;
            let r = Report::parse(&report)?;
            let generation = verify_signature(&report, &vcek)?;
            r.check(policy, expect)?;
            Ok(Verified {
                kind: format!("sev-snp ({generation})"),
                measurement: hex::encode(r.measurement),
                chip: hex::encode(&r.chip_id[..8]),
                reported_tcb: format!("{:016x}", r.reported_tcb),
            })
        }
    }
}

/// The fields of an SEV-SNP attestation report we check (AMD SEV-SNP ABI, ATTESTATION_REPORT).
#[derive(Debug)]
pub struct Report {
    pub version: u32,
    pub policy: u64,
    pub report_data: [u8; 64],
    pub measurement: [u8; 48],
    pub reported_tcb: u64,
    pub chip_id: [u8; 64],
}

impl Report {
    pub fn parse(b: &[u8]) -> Result<Self> {
        ensure!(b.len() == REPORT_LEN, "report is {} bytes, want {REPORT_LEN}", b.len());
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        Ok(Report {
            version: u32_at(0x00),
            policy: u64_at(0x08),
            report_data: b[0x50..0x90].try_into().unwrap(),
            measurement: b[0x90..0xC0].try_into().unwrap(),
            reported_tcb: u64_at(0x180),
            chip_id: b[0x1A0..0x1E0].try_into().unwrap(),
        })
    }

    fn check(&self, policy: &Policy, expect: &[u8; 64]) -> Result<()> {
        ensure!(self.version >= 2, "unsupported report version {}", self.version);
        ensure!(self.policy & POLICY_DEBUG == 0, "guest policy allows debugging");
        ensure!(self.policy & POLICY_MIGRATE_MA == 0, "guest policy allows a migration agent");
        let m = hex::encode(self.measurement);
        ensure!(
            policy.measurements.iter().any(|a| a.eq_ignore_ascii_case(&m)),
            "runner image measurement {m} is not on this server's allow-list"
        );
        ensure!(
            &self.report_data == expect,
            "report does not bind this lease's channel keys"
        );
        Ok(())
    }
}

/// Check VCEK <- ASK <- ARK and the report signature. Returns the CPU generation.
fn verify_signature(report: &[u8], vcek_der: &[u8]) -> Result<&'static str> {
    let vcek = x509_cert::Certificate::from_der(vcek_der).context("VCEK is not a DER certificate")?;
    let generation = AMD_CHAINS
        .iter()
        .find(|(_, pem)| {
            let Ok((ask, ark)) = parse_chain(pem) else { return false };
            rsa_pss_signs(&ark, &ark).is_ok()
                && rsa_pss_signs(&ark, &ask).is_ok()
                && rsa_pss_signs(&ask, &vcek).is_ok()
        })
        .map(|(g, _)| *g)
        .ok_or_else(|| anyhow!("VCEK is not signed by any AMD root this server knows"))?;

    let key = p384::ecdsa::VerifyingKey::from_sec1_bytes(
        vcek.tbs_certificate()
            .subject_public_key_info()
            .subject_public_key
            .raw_bytes(),
    )
    .map_err(|e| anyhow!("VCEK public key: {e}"))?;
    let algo = u32::from_le_bytes(report[0x34..0x38].try_into().unwrap());
    ensure!(algo == 1, "unsupported report signature algorithm {algo}");
    // r and s are stored little-endian in 72-byte fields; P-384 scalars are the low 48 bytes.
    let be = |o: usize| -> Vec<u8> { report[o..o + 48].iter().rev().copied().collect() };
    let mut rs = be(0x2A0);
    rs.extend(be(0x2A0 + 72));
    let sig = p384::ecdsa::Signature::from_slice(&rs).map_err(|e| anyhow!("signature: {e}"))?;
    key.verify(&report[..SIGNED_LEN], &sig)
        .map_err(|_| anyhow!("report signature does not match the VCEK"))?;
    Ok(generation)
}

fn parse_chain(pem: &str) -> Result<(x509_cert::Certificate, x509_cert::Certificate)> {
    let mut certs = pem
        .split_inclusive("-----END CERTIFICATE-----\n")
        .filter(|p| p.contains("BEGIN"))
        .map(|p| x509_cert::Certificate::from_pem(p.trim_start()).map_err(|e| anyhow!("{e}")));
    let ask = certs.next().ok_or_else(|| anyhow!("empty chain"))??;
    let ark = certs.next().ok_or_else(|| anyhow!("chain has no ARK"))??;
    Ok((ask, ark))
}

/// AMD signs ARK, ASK and VCEK with RSASSA-PSS over SHA-384.
fn rsa_pss_signs(signer: &x509_cert::Certificate, signee: &x509_cert::Certificate) -> Result<()> {
    use x509_cert::der::referenced::OwnedToRef;
    let spki = signer.tbs_certificate().subject_public_key_info().owned_to_ref();
    let key = rsa::PublicKey::try_from(spki).map_err(|e| anyhow!("signer key: {e}"))?;
    let key = rsa::pss::VerifyingKey::<sha2::Sha384>::new(key);
    let msg = signee.tbs_certificate().to_der()?;
    let sig = rsa::pss::Signature::try_from(signee.signature().raw_bytes())
        .map_err(|e| anyhow!("signature: {e}"))?;
    key.verify(&msg, &sig).map_err(|_| anyhow!("bad certificate signature"))
}

/// Inside the VM: ask the CPU for a report over `data` through the kernel's configfs-tsm ABI,
/// which serves SEV-SNP (`sev_guest`) and TDX (`tdx_guest`) alike.
pub fn tsm_report(data: &[u8; 64]) -> Result<(String, Vec<u8>)> {
    let base = std::path::Path::new("/sys/kernel/config/tsm/report");
    ensure!(base.exists(), "no configfs-tsm at {} (not a confidential VM?)", base.display());
    let dir = base.join(format!("moloko-{}", std::process::id()));
    std::fs::create_dir(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let result = (|| {
        std::fs::write(dir.join("inblob"), data)?;
        let provider = std::fs::read_to_string(dir.join("provider"))?.trim().to_string();
        let out = std::fs::read(dir.join("outblob"))?;
        anyhow::Ok((provider, out))
    })();
    let _ = std::fs::remove_dir(&dir);
    result
}

/// The chip's VCEK from AMD's key distribution service, for the TCB in `report`.
/// Milan and Genoa address the chip by its full 64-byte id.
pub fn fetch_vcek(product: &str, report: &[u8]) -> Result<Vec<u8>> {
    let r = Report::parse(report)?;
    let t = r.reported_tcb.to_le_bytes();
    let url = format!(
        "https://kdsintf.amd.com/vcek/v1/{product}/{}?blSPL={}&teeSPL={}&snpSPL={}&ucodeSPL={}",
        hex::encode(r.chip_id),
        t[0],
        t[1],
        t[6],
        t[7]
    );
    let out = std::process::Command::new("curl")
        .args(["-fsSL", "--retry", "3", &url])
        .output()
        .context("run curl")?;
    if !out.status.success() {
        bail!("fetch VCEK from {url}: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real Milan report and its VCEK, from the virtee/sev test suite (Apache-2.0).
    const REPORT: &str = include_str!("testdata/report_milan.hex");
    const VCEK: &[u8] = include_bytes!("testdata/vcek_milan.der");

    fn sample() -> (Vec<u8>, Report) {
        let b = hex::decode(REPORT.trim()).unwrap();
        let r = Report::parse(&b).unwrap();
        (b, r)
    }

    fn policy_for(r: &Report) -> Policy {
        Policy { measurements: vec![hex::encode(r.measurement)], insecure_dev: false }
    }

    #[test]
    fn real_milan_report_verifies() {
        let (b, r) = sample();
        assert_eq!(verify_signature(&b, VCEK).unwrap(), "Milan");
        let ev = Evidence::SevSnp { report: hex::encode(&b), vcek: hex::encode(VCEK) };
        let v = verify(&policy_for(&r), &ev, &r.report_data).unwrap();
        assert_eq!(v.kind, "sev-snp (Milan)");
        assert_eq!(v.measurement, hex::encode(r.measurement));
    }

    #[test]
    fn tampered_report_is_refused() {
        let (mut b, _) = sample();
        b[0x90] ^= 1; // flip a measurement bit
        assert!(verify_signature(&b, VCEK).is_err());
    }

    #[test]
    fn tampered_vcek_is_refused() {
        let (b, _) = sample();
        let mut v = VCEK.to_vec();
        v[40] ^= 0xff;
        assert!(verify_signature(&b, &v).is_err());
    }

    #[test]
    fn unknown_measurement_is_refused() {
        let (b, r) = sample();
        let ev = Evidence::SevSnp { report: hex::encode(&b), vcek: hex::encode(VCEK) };
        let p = Policy { measurements: vec!["00".repeat(48)], insecure_dev: false };
        let e = verify(&p, &ev, &r.report_data).unwrap_err().to_string();
        assert!(e.contains("allow-list"), "{e}");
    }

    #[test]
    fn unbound_keys_are_refused() {
        let (b, r) = sample();
        let ev = Evidence::SevSnp { report: hex::encode(&b), vcek: hex::encode(VCEK) };
        let e = verify(&policy_for(&r), &ev, &[7; 64]).unwrap_err().to_string();
        assert!(e.contains("bind"), "{e}");
    }

    #[test]
    fn dev_evidence_needs_the_flag() {
        assert!(verify(&Policy::default(), &Evidence::Dev, &[0; 64]).is_err());
        let p = Policy { insecure_dev: true, ..Default::default() };
        assert!(verify(&p, &Evidence::Dev, &[0; 64]).is_ok());
    }
}
