//! Names for what SpecTrawl decodes: talkgroup and radio aliases, site and
//! network names, imported from other tools' data files.
//!
//! - DSD+ (`DSDPlus.groups`, `.radios`, `.sites`, `.networks`): keyed by
//!   protocol and DSD+ network ID ("BEE00.2A5" = WACN.SYSID in hex for P25).
//! - trunk-recorder: `config.json` (systems with control channels and their
//!   talkgroup/unit-tag files) or the CSVs on their own.
//!
//! P25 talkgroup lookups try, in order: DSD+ entries for the exact network,
//! a trunk-recorder system whose control channel matches, then standalone
//! CSV imports.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GroupInfo {
    pub alias: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tag: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub category: String,
}

impl GroupInfo {
    /// Best short name: the alias, else the description.
    pub fn name(&self) -> &str {
        if self.alias.is_empty() { &self.description } else { &self.alias }
    }
}

/// A trunk-recorder system from config.json.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrSystem {
    pub short_name: String,
    pub kind: String,
    pub control_hz: Vec<f64>,
    pub groups: HashMap<u32, GroupInfo>,
    pub units: HashMap<u32, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Catalog {
    /// key: "PROTO|NET|ID" (DSD+ protocol name and network ID, uppercased).
    pub groups: HashMap<String, GroupInfo>,
    pub radios: HashMap<String, String>,
    /// key: "PROTO|NET|RFSS.SITE" with numbers normalised ("2.21").
    pub sites: HashMap<String, String>,
    /// key: "PROTO|NET".
    pub networks: HashMap<String, String>,
    pub tr_systems: Vec<TrSystem>,
    /// Talkgroups/units imported from CSVs without a config.json.
    pub loose_groups: HashMap<u32, GroupInfo>,
    pub loose_units: HashMap<u32, String>,
    /// What has been imported, for display.
    pub sources: Vec<String>,
}

/// What an import added.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub groups: usize,
    pub radios: usize,
    pub sites: usize,
    pub networks: usize,
    pub systems: usize,
}

impl std::fmt::Display for ImportSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();
        for (n, what) in [
            (self.groups, "talkgroups"),
            (self.radios, "radios"),
            (self.sites, "sites"),
            (self.networks, "networks"),
            (self.systems, "systems"),
        ] {
            if n > 0 {
                parts.push(format!("{n} {what}"));
            }
        }
        if parts.is_empty() { write!(f, "nothing new") } else { write!(f, "{}", parts.join(", ")) }
    }
}

fn key(parts: &[&str]) -> String {
    parts.iter().map(|p| p.trim().to_ascii_uppercase()).collect::<Vec<_>>().join("|")
}

/// P25 network ID as DSD+ writes it: WACN (5 hex) . SYSID (3 hex).
pub fn p25_net(wacn: u32, system: u16) -> String {
    format!("{wacn:05X}.{system:03X}")
}

/// Normalises "2.021" / "2.21" / "021" to "2.21" / "21".
fn site_key(s: &str) -> String {
    s.trim()
        .split('.')
        .map(|p| p.trim().parse::<u64>().map(|v| v.to_string()).unwrap_or_else(|_| p.trim().to_string()))
        .collect::<Vec<_>>()
        .join(".")
}

/// Splits one CSV-ish line on commas outside double quotes; trims fields and
/// strips their quotes. DSD+ prefixes auto-learned aliases with `*`.
pub fn split_fields(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().map(|f| f.trim().trim_start_matches('*').trim().to_string()).collect()
}

fn data_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with(';') && !l.starts_with("//") && !l.starts_with('#'))
}

impl Catalog {
    // ---------------------------------------------------------------- DSD+ ----

    /// `protocol, networkID, group, priority, override, hits, timestamp, "alias"`
    pub fn import_dsdplus_groups(&mut self, text: &str) -> usize {
        let mut n = 0;
        for l in data_lines(text) {
            let f = split_fields(l);
            if f.len() < 8 || f[7].is_empty() {
                continue;
            }
            let Ok(id) = f[2].parse::<i64>() else { continue };
            self.groups.insert(key(&[&f[0], &f[1], &id.to_string()]), GroupInfo { alias: f[7].clone(), ..Default::default() });
            n += 1;
        }
        n
    }

    /// `protocol, networkID, group, radio, priority, override, hits, timestamp, "alias" [, "talker alias", checksum]`
    pub fn import_dsdplus_radios(&mut self, text: &str) -> usize {
        let mut n = 0;
        for l in data_lines(text) {
            let f = split_fields(l);
            if f.len() < 9 {
                continue;
            }
            // Prefer the alias; fall back to the P25 talker alias.
            let alias = if !f[8].is_empty() { f[8].clone() } else { f.get(9).cloned().unwrap_or_default() };
            let Ok(id) = f[3].parse::<i64>() else { continue };
            if alias.is_empty() {
                continue;
            }
            self.radios.insert(key(&[&f[0], &f[1], &id.to_string()]), alias);
            n += 1;
        }
        n
    }

    /// `protocol, networkID, siteNumber, "site name"`
    pub fn import_dsdplus_sites(&mut self, text: &str) -> usize {
        let mut n = 0;
        for l in data_lines(text) {
            let f = split_fields(l);
            if f.len() < 4 || f[3].is_empty() {
                continue;
            }
            self.sites.insert(key(&[&f[0], &f[1], &site_key(&f[2])]), f[3].clone());
            n += 1;
        }
        n
    }

    /// `protocol, networkID, "network name"[, TIIIareaLength]`
    pub fn import_dsdplus_networks(&mut self, text: &str) -> usize {
        let mut n = 0;
        for l in data_lines(text) {
            let f = split_fields(l);
            if f.len() < 3 || f[2].is_empty() {
                continue;
            }
            self.networks.insert(key(&[&f[0], &f[1]]), f[2].clone());
            n += 1;
        }
        n
    }

    /// Imports whichever of DSDPlus.groups/.radios/.sites/.networks exist in `dir`.
    pub fn import_dsdplus_dir(&mut self, dir: &Path) -> std::io::Result<ImportSummary> {
        let read = |name: &str| std::fs::read(dir.join(name)).ok().map(|b| String::from_utf8_lossy(&b).into_owned());
        let mut s = ImportSummary::default();
        if let Some(t) = read("DSDPlus.groups") {
            s.groups = self.import_dsdplus_groups(&t);
        }
        if let Some(t) = read("DSDPlus.radios") {
            s.radios = self.import_dsdplus_radios(&t);
        }
        if let Some(t) = read("DSDPlus.sites") {
            s.sites = self.import_dsdplus_sites(&t);
        }
        if let Some(t) = read("DSDPlus.networks") {
            s.networks = self.import_dsdplus_networks(&t);
        }
        if s == ImportSummary::default() {
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no DSDPlus.groups/.radios/.sites/.networks in that folder"));
        }
        self.sources.push(format!("DSD+ {} ({s})", dir.display()));
        Ok(s)
    }

    // ------------------------------------------------------- trunk-recorder ----

    /// Parses a trunk-recorder talkgroup CSV. With a header row, columns are
    /// found by name ("Decimal", "Alpha Tag", "Description", "Tag",
    /// "Category"); without one, the classic order is assumed:
    /// Decimal, Hex, Alpha Tag, Mode, Description, Tag, Category[, Priority].
    pub fn parse_tr_talkgroups(text: &str) -> HashMap<u32, GroupInfo> {
        let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'));
        let mut out = HashMap::new();
        let Some(first) = lines.next() else { return out };
        let first_f = split_fields(first);
        let header = first_f.first().is_some_and(|f| f.parse::<u32>().is_err());
        let col = |names: &[&str], default: usize| -> usize {
            if !header {
                return default;
            }
            first_f.iter().position(|h| names.iter().any(|n| h.eq_ignore_ascii_case(n))).unwrap_or(usize::MAX)
        };
        let (c_id, c_alpha, c_desc, c_tag, c_cat) = (
            col(&["decimal", "dec", "tgid", "talkgroup"], 0),
            col(&["alpha tag", "alpha", "alphatag"], 2),
            col(&["description"], 4),
            col(&["tag"], 5),
            col(&["category", "group"], 6),
        );
        let get = |f: &[String], i: usize| f.get(i).cloned().unwrap_or_default();
        let rows = if header { None } else { Some(first) }.into_iter().chain(lines);
        for l in rows {
            let f = split_fields(l);
            let Ok(id) = get(&f, c_id).parse::<u32>() else { continue };
            out.insert(
                id,
                GroupInfo { alias: get(&f, c_alpha), description: get(&f, c_desc), tag: get(&f, c_tag), category: get(&f, c_cat) },
            );
        }
        out
    }

    /// trunk-recorder unit tags: `unit_id,tag` (regex rows are skipped).
    pub fn parse_tr_units(text: &str) -> HashMap<u32, String> {
        data_lines(text)
            .filter_map(|l| {
                let f = split_fields(l);
                Some((f.first()?.parse::<u32>().ok()?, f.get(1)?.clone()))
            })
            .filter(|(_, t)| !t.is_empty())
            .collect()
    }

    /// Imports a CSV on its own, guessing talkgroups vs unit tags by width.
    pub fn import_csv(&mut self, path: &Path) -> std::io::Result<ImportSummary> {
        let text = String::from_utf8_lossy(&std::fs::read(path)?).into_owned();
        let widest = text.lines().take(20).map(|l| split_fields(l).len()).max().unwrap_or(0);
        let mut s = ImportSummary::default();
        if widest <= 2 {
            let u = Self::parse_tr_units(&text);
            s.radios = u.len();
            self.loose_units.extend(u);
        } else {
            let g = Self::parse_tr_talkgroups(&text);
            s.groups = g.len();
            self.loose_groups.extend(g);
        }
        self.sources.push(format!("CSV {} ({s})", path.display()));
        Ok(s)
    }

    /// Imports a trunk-recorder config.json: each system with its control
    /// channels and the talkgroup/unit files it references (paths relative
    /// to the config file).
    pub fn import_tr_config(&mut self, path: &Path) -> std::io::Result<ImportSummary> {
        let text = std::fs::read_to_string(path)?;
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let base: PathBuf = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let resolve = |p: &str| {
            let p = PathBuf::from(p);
            if p.is_absolute() { p } else { base.join(p) }
        };
        let mut s = ImportSummary::default();
        for sys in v["systems"].as_array().into_iter().flatten() {
            let mut t = TrSystem {
                short_name: sys["shortName"].as_str().unwrap_or("").to_string(),
                kind: sys["type"].as_str().unwrap_or("").to_string(),
                control_hz: sys["control_channels"].as_array().into_iter().flatten().filter_map(|c| c.as_f64()).collect(),
                ..Default::default()
            };
            if let Some(f) = sys["talkgroupsFile"].as_str() {
                if let Ok(b) = std::fs::read(resolve(f)) {
                    t.groups = Self::parse_tr_talkgroups(&String::from_utf8_lossy(&b));
                }
            }
            if let Some(f) = sys["unitTagsFile"].as_str() {
                if let Ok(b) = std::fs::read(resolve(f)) {
                    t.units = Self::parse_tr_units(&String::from_utf8_lossy(&b));
                }
            }
            s.groups += t.groups.len();
            s.radios += t.units.len();
            s.systems += 1;
            self.tr_systems.retain(|o| o.short_name != t.short_name);
            self.tr_systems.push(t);
        }
        self.sources.push(format!("trunk-recorder {} ({s})", path.display()));
        Ok(s)
    }

    // ------------------------------------------------------------- lookups ----

    fn tr_for_control(&self, control_hz: Option<f64>) -> Option<&TrSystem> {
        let c = control_hz?;
        self.tr_systems.iter().find(|s| s.control_hz.iter().any(|f| (f - c).abs() < 1_000.0))
    }

    /// A P25 talkgroup's name. `control_hz` (the system's control channel)
    /// links it to a trunk-recorder system.
    pub fn p25_group(&self, wacn: Option<u32>, system: Option<u16>, control_hz: Option<f64>, tg: u16) -> Option<&GroupInfo> {
        if let (Some(w), Some(s)) = (wacn, system) {
            if let Some(g) = self.groups.get(&key(&["P25", &p25_net(w, s), &tg.to_string()])) {
                return Some(g);
            }
        }
        self.tr_for_control(control_hz).and_then(|t| t.groups.get(&(tg as u32))).or_else(|| self.loose_groups.get(&(tg as u32)))
    }

    pub fn p25_radio(&self, wacn: Option<u32>, system: Option<u16>, control_hz: Option<f64>, radio: u32) -> Option<&str> {
        if let (Some(w), Some(s)) = (wacn, system) {
            if let Some(r) = self.radios.get(&key(&["P25", &p25_net(w, s), &radio.to_string()])) {
                return Some(r);
            }
        }
        self.tr_for_control(control_hz).and_then(|t| t.units.get(&radio)).or_else(|| self.loose_units.get(&radio)).map(String::as_str)
    }

    pub fn p25_site(&self, wacn: u32, system: u16, rfss: u8, site: u8) -> Option<&str> {
        self.sites.get(&key(&["P25", &p25_net(wacn, system), &format!("{rfss}.{site}")])).map(String::as_str)
    }

    pub fn p25_network(&self, wacn: u32, system: u16) -> Option<&str> {
        self.networks.get(&key(&["P25", &p25_net(wacn, system)])).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
            && self.radios.is_empty()
            && self.sites.is_empty()
            && self.tr_systems.is_empty()
            && self.loose_groups.is_empty()
            && self.loose_units.is_empty()
    }

    // --------------------------------------------------------- persistence ----

    pub fn load(path: &Path) -> Self {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(path, serde_json::to_vec(self).map_err(std::io::Error::other)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GROUPS: &str = r#"; DSD+ 2.523; group records
; line format: protocol, networkID, group, priority, override, hits, timestamp, "group alias"
P25,       BEE00.2A5, 48001,      50,  Normal,      32,  2026/10/08 10:41,  "[PFPD DISP] Pigeon Forge Police Dispatch"
P25,       BEE00.2A5, 99,         50,  Normal,       0,  0000/00/00  0:00,  ""
dPMR,      0,         34,         50,  Normal,       3,  2021/07/09 20:38,  ""
"#;
    const RADIOS: &str = r#"; line format: protocol, networkID, group, radio, priority, override, hits, timestamp, "radio alias" [, "P25 talker alias", checksum]
D-Star,    0,         -1,         1720160526, 50,  Normal,       2,  2022/05/06 13:59,  *"AD8IL/9700"
P25,       BEE00.2A5, -1,         4068001,    50,  Normal,       5,  2026/10/08 10:41,  "PF Unit 12"
P25,       BEE00.2A5, -1,         4068002,    50,  Normal,       5,  2026/10/08 10:41,  "", "TALKER 2", 1234
"#;
    const SITES: &str = r#"; line format: protocol, networkID, siteNumber, "site name"
P25,BEE00.2A5,2.021,"Greentop Mountain"
"#;

    #[test]
    fn dsdplus_files() {
        let mut c = Catalog::default();
        assert_eq!(c.import_dsdplus_groups(GROUPS), 1);
        assert_eq!(c.import_dsdplus_radios(RADIOS), 3);
        assert_eq!(c.import_dsdplus_sites(SITES), 1);
        let (w, s) = (Some(0xBEE00), Some(0x2A5));
        assert_eq!(c.p25_group(w, s, None, 48001).unwrap().name(), "[PFPD DISP] Pigeon Forge Police Dispatch");
        assert!(c.p25_group(w, s, None, 99).is_none());
        assert_eq!(c.p25_radio(w, s, None, 4068001), Some("PF Unit 12"));
        assert_eq!(c.p25_radio(w, s, None, 4068002), Some("TALKER 2"));
        assert_eq!(c.p25_site(0xBEE00, 0x2A5, 2, 21), Some("Greentop Mountain"));
    }

    #[test]
    fn trunk_recorder_csvs() {
        let with_header = "Decimal,Hex,Alpha Tag,Mode,Description,Tag,Category\n48001,bb81,PFPD DISP,D,Pigeon Forge Police Dispatch,Law Dispatch,Sevier County\n";
        let g = Catalog::parse_tr_talkgroups(with_header);
        assert_eq!(g[&48001].alias, "PFPD DISP");
        assert_eq!(g[&48001].category, "Sevier County");
        let classic = "48001,bb81,PFPD DISP,D,\"Pigeon Forge, Police\",Law Dispatch,Sevier County,1\n";
        let g = Catalog::parse_tr_talkgroups(classic);
        assert_eq!(g[&48001].description, "Pigeon Forge, Police");
        let u = Catalog::parse_tr_units("4068001,PF Unit 12\n^40680\\d\\d$,PF\n");
        assert_eq!(u.len(), 1);
    }

    #[test]
    fn trunk_recorder_config_links_by_control_channel() {
        let dir = std::env::temp_dir().join(format!("catalog-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tg.csv"), "48001,bb81,PFPD DISP,D,Pigeon Forge Police Dispatch,Law Dispatch,Sevier\n").unwrap();
        std::fs::write(
            dir.join("config.json"),
            r#"{"systems":[{"shortName":"tacn","type":"p25","control_channels":[769581250],"talkgroupsFile":"tg.csv"}]}"#,
        )
        .unwrap();
        let mut c = Catalog::default();
        let s = c.import_tr_config(&dir.join("config.json")).unwrap();
        assert_eq!((s.systems, s.groups), (1, 1));
        assert_eq!(c.p25_group(Some(0xBEE00), Some(0x2A5), Some(769.58125e6), 48001).unwrap().alias, "PFPD DISP");
        assert!(c.p25_group(None, None, Some(851.0e6), 48001).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
