use chrono::{Local, NaiveDate};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::Manager;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BandwidthStats {
    /// Date of the current daily quota window (YYYY-MM-DD). Kept as `date`
    /// for backwards compatibility with existing bandwidth.json files.
    pub date: String,
    pub up_bytes: u64,
    pub down_bytes: u64,
    #[serde(default = "daily_limit_bytes")]
    pub limit_bytes: u64,
    #[serde(default = "daily_period_name")]
    pub period: String,
}

pub const DAILY_LIMIT_BYTES: u64 = 1000 * 1024 * 1024 * 1024;

fn daily_limit_bytes() -> u64 {
    DAILY_LIMIT_BYTES
}
fn daily_period_name() -> String {
    "daily".to_string()
}

fn canonical_today_date() -> (NaiveDate, String) {
    let today = Local::now().date_naive();
    let formatted = today.format("%Y-%m-%d").to_string();
    (today, formatted)
}

impl Default for BandwidthStats {
    fn default() -> Self {
        let (_, date_str) = canonical_today_date();
        Self {
            date: date_str,
            up_bytes: 0,
            down_bytes: 0,
            limit_bytes: DAILY_LIMIT_BYTES,
            period: daily_period_name(),
        }
    }
}

pub struct BandwidthManager {
    pub file_path: PathBuf,
    pub stats: Mutex<BandwidthStats>,
    pub limit: u64, // Daily upload limit in bytes
}

#[derive(Clone, Copy)]
enum ReservationDirection {
    Upload,
    Download,
}

/// Releases a bandwidth reservation automatically on every error/cancellation
/// path. Successful transfers call `commit` so their bytes remain accounted.
pub struct BandwidthReservation {
    manager: std::sync::Arc<BandwidthManager>,
    bytes: u64,
    direction: ReservationDirection,
    committed: bool,
}

impl BandwidthReservation {
    pub fn upload(manager: std::sync::Arc<BandwidthManager>, bytes: u64) -> Result<Self, String> {
        manager.try_reserve_up(bytes)?;
        Ok(Self {
            manager,
            bytes,
            direction: ReservationDirection::Upload,
            committed: false,
        })
    }

    pub fn download(manager: std::sync::Arc<BandwidthManager>, bytes: u64) -> Result<Self, String> {
        manager.try_reserve_down(bytes)?;
        Ok(Self {
            manager,
            bytes,
            direction: ReservationDirection::Download,
            committed: false,
        })
    }

    pub fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for BandwidthReservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        match self.direction {
            ReservationDirection::Upload => self.manager.release_up(self.bytes),
            ReservationDirection::Download => self.manager.release_down(self.bytes),
        }
    }
}

impl BandwidthManager {
    pub fn new(app_handle: &tauri::AppHandle) -> Self {
        // Resolve app data directory
        let app_data_dir = app_handle
            .path()
            .app_data_dir()
            .unwrap_or_else(|_| PathBuf::from("data"));

        if !app_data_dir.exists() {
            let _ = std::fs::create_dir_all(&app_data_dir);
        }
        let file_path = app_data_dir.join("bandwidth.json");

        let stats = if file_path.exists() {
            let content = fs::read_to_string(&file_path).unwrap_or_default();
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            BandwidthStats::default()
        };

        let manager = Self {
            file_path,
            stats: Mutex::new(stats),
            limit: DAILY_LIMIT_BYTES,
        };
        manager.check_and_reset();
        manager
    }

    pub fn check_and_reset(&self) {
        let (_, canonical_date) = canonical_today_date();
        let mut stats = self.stats.lock().unwrap();
        let previous = stats.clone();
        let is_current_day = stats.date == canonical_date && stats.period == "daily";
        let metadata_changed = stats.limit_bytes != self.limit || stats.period != "daily";

        if !is_current_day {
            println!(
                "[Bandwidth] New day detected. Resetting stats. Old period: {}, New period: {}",
                stats.date, canonical_date
            );
            stats.up_bytes = 0;
            stats.down_bytes = 0;
            stats.date = canonical_date.clone();
        }
        stats.date = canonical_date;
        stats.limit_bytes = self.limit;
        stats.period = daily_period_name();

        if !is_current_day || metadata_changed {
            if let Err(error) = self.save_locked(&stats) {
                *stats = previous;
                log::error!("Unable to persist the bandwidth period rollover: {error}");
            }
        }
    }

    pub fn can_transfer(&self, bytes: u64) -> Result<(), String> {
        self.check_and_reset();
        let stats = self.stats.lock().unwrap();
        let total = stats
            .up_bytes
            .checked_add(bytes)
            .ok_or_else(|| "Bandwidth accounting overflowed".to_string())?;
        if total > self.limit {
            return Err(format!(
                "Daily upload bandwidth limit ({}) exceeded! Used: {}",
                self.format_bytes(self.limit),
                self.format_bytes(total)
            ));
        }
        Ok(())
    }

    /// Atomically check the daily upload limit AND reserve bandwidth for an upload.
    /// Call release_up() or drop BandwidthReservation if the transfer fails to avoid consuming quota.
    pub fn try_reserve_up(&self, bytes: u64) -> Result<(), String> {
        self.check_and_reset();
        let mut stats = self.stats.lock().unwrap();
        let total = stats
            .up_bytes
            .checked_add(bytes)
            .ok_or_else(|| "Bandwidth accounting overflowed".to_string())?;
        if total > self.limit {
            return Err(format!(
                "Daily upload bandwidth limit ({}) exceeded! Used: {}",
                self.format_bytes(self.limit),
                self.format_bytes(total)
            ));
        }
        let previous = stats.clone();
        stats.up_bytes = total;
        if let Err(error) = self.save_locked(&stats) {
            *stats = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Record download bandwidth for informational purposes.
    /// Downloads do NOT consume or block against the upload quota.
    pub fn try_reserve_down(&self, bytes: u64) -> Result<(), String> {
        self.check_and_reset();
        let mut stats = self.stats.lock().unwrap();
        let previous = stats.clone();
        stats.down_bytes = stats
            .down_bytes
            .checked_add(bytes)
            .ok_or_else(|| "Bandwidth accounting overflowed".to_string())?;
        if let Err(error) = self.save_locked(&stats) {
            *stats = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Release reserved upload bandwidth after a failed transfer.
    pub fn release_up(&self, bytes: u64) {
        let mut stats = self.stats.lock().unwrap();
        let previous = stats.clone();
        stats.up_bytes = stats.up_bytes.saturating_sub(bytes);
        if let Err(error) = self.save_locked(&stats) {
            *stats = previous;
            log::error!("Unable to release an upload bandwidth reservation: {error}");
        }
    }

    /// Release reserved download bandwidth after a cancelled/failed transfer.
    pub fn release_down(&self, bytes: u64) {
        let mut stats = self.stats.lock().unwrap();
        let previous = stats.clone();
        stats.down_bytes = stats.down_bytes.saturating_sub(bytes);
        if let Err(error) = self.save_locked(&stats) {
            *stats = previous;
            log::error!("Unable to release a download bandwidth reservation: {error}");
        }
    }

    pub fn add_up(&self, bytes: u64) {
        self.check_and_reset();
        let mut stats = self.stats.lock().unwrap();
        let previous = stats.clone();
        let Some(total) = stats.up_bytes.checked_add(bytes) else {
            log::error!("Upload bandwidth accounting overflowed");
            return;
        };
        stats.up_bytes = total;
        if let Err(error) = self.save_locked(&stats) {
            *stats = previous;
            log::error!("Unable to persist upload bandwidth: {error}");
        }
    }

    pub fn add_down(&self, bytes: u64) {
        self.check_and_reset();
        let mut stats = self.stats.lock().unwrap();
        let previous = stats.clone();
        let Some(total) = stats.down_bytes.checked_add(bytes) else {
            log::error!("Download bandwidth accounting overflowed");
            return;
        };
        stats.down_bytes = total;
        if let Err(error) = self.save_locked(&stats) {
            *stats = previous;
            log::error!("Unable to persist download bandwidth: {error}");
        }
    }

    fn save_locked(&self, stats: &BandwidthStats) -> Result<(), String> {
        persist_stats_atomically(&self.file_path, stats)
    }

    pub fn get_stats(&self) -> BandwidthStats {
        self.check_and_reset();
        self.stats.lock().unwrap().clone()
    }

    fn format_bytes(&self, bytes: u64) -> String {
        const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
        let mut v = bytes as f64;
        let mut i = 0;
        while v >= 1024.0 && i < UNITS.len() - 1 {
            v /= 1024.0;
            i += 1;
        }
        format!("{:.2} {}", v, UNITS[i])
    }
}

fn persist_stats_atomically(path: &Path, stats: &BandwidthStats) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Bandwidth data path has no parent".to_string())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = parent.join(format!(".bandwidth.{}.tmp", uuid::Uuid::new_v4()));
    let bytes = serde_json::to_vec_pretty(stats).map_err(|error| error.to_string())?;
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        file.write_all(&bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        atomic_replace(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(not(target_os = "windows"))]
fn atomic_replace(source: &Path, destination: &Path) -> Result<(), String> {
    fs::rename(source, destination).map_err(|error| error.to_string())
}

#[cfg(target_os = "windows")]
fn atomic_replace(source: &Path, destination: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
    if !destination.exists() {
        return fs::rename(source, destination).map_err(|error| error.to_string());
    }
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let source: Vec<u16> = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let replaced = unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            source.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if replaced == 0 {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_manager_with_limit(label: &str, limit: u64) -> std::sync::Arc<BandwidthManager> {
        let path = std::env::temp_dir().join(format!(
            "telegram-drive-bandwidth-{label}-{}.json",
            uuid::Uuid::new_v4()
        ));
        std::sync::Arc::new(BandwidthManager {
            file_path: path,
            stats: Mutex::new(BandwidthStats::default()),
            limit,
        })
    }

    fn test_manager(label: &str) -> std::sync::Arc<BandwidthManager> {
        test_manager_with_limit(label, DAILY_LIMIT_BYTES)
    }

    #[test]
    fn daily_reset_resets_on_new_date() {
        let manager = test_manager("daily_reset");
        {
            let mut stats = manager.stats.lock().unwrap();
            stats.date = "2026-01-01".to_string();
            stats.up_bytes = 50_000;
            stats.down_bytes = 20_000;
        }
        manager.check_and_reset();
        let stats = manager.get_stats();
        let (_, today_str) = canonical_today_date();
        assert_eq!(stats.date, today_str);
        assert_eq!(stats.up_bytes, 0);
        assert_eq!(stats.down_bytes, 0);
        assert_eq!(stats.limit_bytes, DAILY_LIMIT_BYTES);
        assert_eq!(stats.period, "daily");
        let _ = std::fs::remove_file(&manager.file_path);
    }

    #[test]
    fn same_day_preserves_upload_usage() {
        let manager = test_manager("same_day");
        manager.add_up(1024 * 1024);
        manager.check_and_reset();
        let stats = manager.get_stats();
        assert_eq!(stats.up_bytes, 1024 * 1024);
        let _ = std::fs::remove_file(&manager.file_path);
    }

    #[test]
    fn failed_upload_reservation_releases_quota_on_drop() {
        let manager = test_manager("upload_release");
        {
            let _reservation = BandwidthReservation::upload(manager.clone(), 8192).unwrap();
            assert_eq!(manager.get_stats().up_bytes, 8192);
        }
        assert_eq!(manager.get_stats().up_bytes, 0);
        let _ = std::fs::remove_file(&manager.file_path);
    }

    #[test]
    fn committed_upload_reservation_remains_accounted() {
        let manager = test_manager("upload_commit");
        {
            let mut reservation = BandwidthReservation::upload(manager.clone(), 8192).unwrap();
            reservation.commit();
        }
        assert_eq!(manager.get_stats().up_bytes, 8192);
        let _ = std::fs::remove_file(&manager.file_path);
    }

    #[test]
    fn downloads_do_not_consume_upload_quota() {
        let manager = test_manager_with_limit("dl_quota", 10_000);
        // Add 50,000 bytes download (far more than limit)
        manager.add_down(50_000);
        // Upload of 5,000 bytes within the 10,000 limit should succeed
        assert!(manager.can_transfer(5_000).is_ok());
        assert!(manager.try_reserve_up(5_000).is_ok());
        // Upload exceeding remaining 5,000 should fail
        assert!(manager.try_reserve_up(6_000).is_err());
        let _ = std::fs::remove_file(&manager.file_path);
    }

    #[test]
    fn retry_does_not_double_count() {
        let manager = test_manager("retry_count");
        let file_size = 50_000_000u64;

        // Attempt 1: Upload fails / drops without commit
        {
            let _attempt1 = BandwidthReservation::upload(manager.clone(), file_size).unwrap();
            assert_eq!(manager.get_stats().up_bytes, file_size);
            // simulated failure here (scope exits)
        }
        assert_eq!(manager.get_stats().up_bytes, 0);

        // Attempt 2 (Retry): Upload succeeds and commits
        {
            let mut attempt2 = BandwidthReservation::upload(manager.clone(), file_size).unwrap();
            attempt2.commit();
        }
        assert_eq!(manager.get_stats().up_bytes, file_size);
        let _ = std::fs::remove_file(&manager.file_path);
    }

    #[test]
    fn concurrent_reservations_cannot_overbook_the_limit() {
        let manager = test_manager_with_limit("concurrent", 1_000);
        let mut workers = Vec::new();
        for _ in 0..20 {
            let manager = manager.clone();
            workers.push(std::thread::spawn(move || {
                manager.try_reserve_up(100).is_ok()
            }));
        }
        let accepted = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|accepted| *accepted)
            .count();
        assert_eq!(accepted, 10);
        assert_eq!(manager.get_stats().up_bytes, 1_000);
        let persisted: BandwidthStats =
            serde_json::from_slice(&std::fs::read(&manager.file_path).unwrap()).unwrap();
        assert_eq!(persisted.up_bytes, 1_000);
        let _ = std::fs::remove_file(&manager.file_path);
    }

    #[test]
    fn daily_migration_from_old_weekly_bandwidth_json_is_safe() {
        let manager = test_manager("migration");
        // Simulate reading an old weekly json
        let old_json = r#"{"date":"2026-08-24","up_bytes":150000,"down_bytes":250000,"limit_bytes":268435456000,"period":"weekly"}"#;
        std::fs::write(&manager.file_path, old_json).unwrap();
        let parsed: BandwidthStats = serde_json::from_str(old_json).unwrap();
        *manager.stats.lock().unwrap() = parsed;

        manager.check_and_reset();
        let stats = manager.get_stats();
        let (_, today_str) = canonical_today_date();
        assert_eq!(stats.date, today_str);
        assert_eq!(stats.period, "daily");
        assert_eq!(stats.limit_bytes, DAILY_LIMIT_BYTES);
        assert_eq!(stats.up_bytes, 0);
        assert_eq!(stats.down_bytes, 0);
        let _ = std::fs::remove_file(&manager.file_path);
    }
}
