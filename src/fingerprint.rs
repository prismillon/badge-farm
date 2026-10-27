use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::Command;

const CAREERS: &str = "https://discord.com/careers";
const PREFIX: usize = 0x10000;

fn sha(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn xor(mut out: Vec<u8>, user_id: &str) -> String {
    let key = sha(format!("{user_id}{CAREERS}").as_bytes());
    for i in 1..out.len() {
        out[i] ^= key[(i - 1) & 31];
    }
    to_hex(&out)
}

fn payload(
    flags: u8,
    file_size: u64,
    timestamp: u32,
    size_of_image: u32,
    header: [u8; 32],
    machine_user: [u8; 32],
    section: [u8; 32],
    cert: [u8; 32],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(146);
    out.push(1);
    out.push(flags);
    out.extend_from_slice(&file_size.to_le_bytes());
    out.extend_from_slice(&timestamp.to_le_bytes());
    out.extend_from_slice(&size_of_image.to_le_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(&machine_user);
    out.extend_from_slice(&section);
    out.extend_from_slice(&cert);
    out
}

fn machine_user(machine_guid: &str, user_id: &str) -> [u8; 32] {
    sha(format!("{machine_guid}{user_id}").as_bytes())
}

pub fn machine_guid() -> String {
    if let Ok(value) = std::env::var("BADGE_MACHINE_GUID") {
        if !value.trim().is_empty() {
            return value.trim().to_string();
        }
    }

    let out = Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Cryptography",
            "/v",
            "MachineGuid",
        ])
        .output();

    if let Ok(out) = out {
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(line) = text.lines().find(|l| l.contains("MachineGuid")) {
            if let Some(value) = line.split_whitespace().last() {
                if value.len() >= 32 {
                    return value.to_string();
                }
            }
        }
    }

    if let Ok(text) = std::fs::read_to_string("/etc/machine-id") {
        let value = text.trim();
        if !value.is_empty() {
            return value.to_string();
        }
    }

    let path = std::env::var("HOME")
        .map(|home| format!("{home}/.badge-farm-machine-guid"))
        .unwrap_or_else(|_| ".badge-farm-machine-guid".to_string());

    if let Ok(text) = std::fs::read_to_string(&path) {
        let value = text.trim();
        if !value.is_empty() {
            return value.to_string();
        }
    }

    let value = uuid::Uuid::new_v4().to_string();
    let _ = std::fs::write(&path, &value);
    value
}

pub fn from_file(path: &Path, machine_guid: &str, user_id: &str) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    let file_size = data.len() as u64;
    let window = &data[..data.len().min(PREFIX)];

    if window.len() < 0x40 || u16::from_le_bytes([window[0], window[1]]) != 0x5A4D {
        return None;
    }

    let pe = u32::from_le_bytes([window[0x3C], window[0x3D], window[0x3E], window[0x3F]]) as usize;
    if pe + 24 > window.len() || &window[pe..pe + 4] != b"PE\0\0" {
        return None;
    }

    let header = pe + 4;
    let sections = u16::from_le_bytes([window[header + 2], window[header + 3]]) as usize;
    let timestamp = u32::from_le_bytes([
        window[header + 4],
        window[header + 5],
        window[header + 6],
        window[header + 7],
    ]);
    let opt_size = u16::from_le_bytes([window[header + 16], window[header + 17]]) as usize;
    let opt = header + 20;

    if u16::from_le_bytes([window[opt], window[opt + 1]]) != 0x20B {
        return None;
    }

    let size_of_image = u32::from_le_bytes([
        window[opt + 56],
        window[opt + 57],
        window[opt + 58],
        window[opt + 59],
    ]);

    let table = opt + opt_size;
    let table_end = table + sections * 40;
    if table_end > window.len() {
        return None;
    }

    let header_hash = sha(&window[..table]);
    let section_hash = sha(&window[table..table_end]);

    let out = payload(
        2,
        file_size,
        timestamp,
        size_of_image,
        header_hash,
        machine_user(machine_guid, user_id),
        section_hash,
        [0u8; 32],
    );

    Some(xor(out, user_id))
}

pub fn synthetic(game_id: &str, machine_guid: &str, user_id: &str) -> String {
    let seed = sha(format!("exec:{game_id}:{machine_guid}:{user_id}").as_bytes());
    let seed_hex = to_hex(&seed);

    let a = u64::from_le_bytes(seed[0..8].try_into().unwrap());
    let b = u32::from_le_bytes(seed[8..12].try_into().unwrap());
    let c = u32::from_le_bytes(seed[12..16].try_into().unwrap());

    let file_size = 4 * 1024 * 1024 + a % (120 * 1024 * 1024);
    let timestamp = 1_451_606_400 + b % 283_000_000;
    let size_of_image = ((file_size + 0x2000 + (c as u64 % 0x300000)) & !0xFFF) as u32;

    let header = sha(format!("h:{seed_hex}").as_bytes());
    let section = sha(format!("s:{seed_hex}").as_bytes());

    let out = payload(
        2,
        file_size,
        timestamp,
        size_of_image,
        header,
        machine_user(machine_guid, user_id),
        section,
        [0u8; 32],
    );

    xor(out, user_id)
}

pub fn for_game(game_id: &str, executable: &str, machine_guid: &str, user_id: &str) -> String {
    let path = Path::new(executable);
    if path.is_absolute() {
        if let Some(value) = from_file(path, machine_guid, user_id) {
            return value;
        }
    }
    synthetic(game_id, machine_guid, user_id)
}
