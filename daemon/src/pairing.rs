//! `screx unpair`: lists or removes paired devices. Pairing itself happens during
//! authentication in `quic_transport`.
use crate::{
    platform::shim::config_dir,
    quic_transport::{PairedClient, Pairings},
};
use anyhow::Result;

/// The short ID shown in the list and the logs.
fn short_id(device_id: &str) -> &str {
    &device_id[..device_id.len().min(5)]
}

/// Unix seconds as "YYYY-MM-DD HH:MM UTC".
fn format_paired_at(secs: u64) -> String {
    let secs = secs as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant), proleptic Gregorian calendar.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
}

fn print_devices(pairings: &Pairings) {
    let mut entries: Vec<(&String, &PairedClient)> = pairings.clients.iter().collect();
    entries.sort_by_key(|(_, device)| device.paired_at);
    for (id, device) in entries {
        println!(
            "  {}  {}  paired {}",
            short_id(id),
            device.name,
            format_paired_at(device.paired_at)
        );
    }
}

/// Lists pairings, or removes one by ID. The 5-character short ID shown in the list and the
/// logs is enough, as long as it matches only one pairing.
pub fn run_unpair(device_id: Option<&str>) -> Result<()> {
    let mut pairings = Pairings::load(&config_dir())?;
    match device_id {
        Some("--all") => {
            let count = pairings.clients.len();
            pairings.clients.clear();
            pairings.save()?;
            println!("Removed all {count} paired device(s).");
        }
        Some(prefix) => {
            let prefix = prefix.to_ascii_lowercase();
            let matches: Vec<String> = pairings
                .clients
                .keys()
                .filter(|id| id.starts_with(&prefix))
                .cloned()
                .collect();
            match matches.as_slice() {
                [id] => {
                    let device = pairings.clients.remove(id).unwrap();
                    pairings.save()?;
                    println!("Unpaired {} ({}).", device.name, short_id(id));
                }
                [] => {
                    println!("No paired device matches {prefix}.");
                    print_devices(&pairings);
                }
                _ => {
                    println!("{prefix} matches more than one device; use more characters.");
                    print_devices(&pairings);
                }
            }
        }
        None => {
            if pairings.clients.is_empty() {
                println!("No paired devices.");
            } else {
                println!("Paired devices:");
                print_devices(&pairings);
                println!("\nUsage: screx unpair <id> or screx unpair --all");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{format_paired_at, short_id};

    #[test]
    fn paired_at_is_readable_utc() {
        assert_eq!(format_paired_at(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_paired_at(951_782_400), "2000-02-29 00:00 UTC");
        assert_eq!(format_paired_at(1_794_811_027), "2026-11-16 06:37 UTC");
    }

    #[test]
    fn short_id_is_five_characters() {
        assert_eq!(short_id("a3f9c0112233"), "a3f9c");
        assert_eq!(short_id("ab"), "ab");
    }
}
