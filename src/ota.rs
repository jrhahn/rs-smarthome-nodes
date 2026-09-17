//! Updating a node without walking to it.
//!
//! The chip has two application slots and a small selector sector between them;
//! the ESP-IDF bootloader we already boot through reads that selector and runs
//! whichever slot it names. So an update is: write the slot that is *not*
//! running, point the selector at it, reboot. Nothing here talks to the network
//! — [`crate::http`] fetches the bytes and hands them over a chunk at a time.
//!
//! **The dangerous half is not the download.** An image that fails to build a
//! working node still boots: a wrong SSID, a broker that moved, a panic after
//! association — all of them produce a board that runs, says nothing, and
//! cannot be told anything. On `terrasse` that means a ladder. So this module
//! is built around one rule:
//!
//! > A new slot is *unconfirmed* until the node has completed a publish round on
//! > it. Three boots without that and the selector is pointed back at the slot
//! > that worked.
//!
//! The counting is done here, in the application, and deliberately not left to
//! the bootloader's own rollback: that is a bootloader *build* option
//! (`CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE`), and the bootloader on these boards
//! is the prebuilt one `espflash` ships. Assuming a rollback that is not
//! compiled in is exactly the mistake that strands a node.
//!
//! What lives where, all of it fixed by `partitions.csv`:
//!
//! | Offset | Contents |
//! | --- | --- |
//! | `0x9000`–`0xB000` | the config, identity and Wi-Fi blobs of [`crate::config`] |
//! | `0xC000` | this module's bookkeeping: which slot is unconfirmed, and for how many boots |
//! | `0xD000` | `otadata`, two copies, the bootloader's own selector |
//! | `0x10000` | `ota_0` |
//! | `0x200000` | `ota_1` |

#[cfg(feature = "hal")]
use embedded_storage::{ReadStorage, Storage};
#[cfg(feature = "hal")]
use esp_storage::FlashStorage;

/// Flash sector size, and the granularity of every erase on this chip.
pub const SECTOR: u32 = 0x1000;

/// Where `otadata` starts. Two sectors: the bootloader keeps a copy in each and
/// takes the one with the higher sequence number, which is what makes switching
/// slots survive a power cut in the middle of it.
pub const OTADATA_OFFSET: u32 = 0xD000;

/// The application slots, in the order the bootloader numbers them.
pub const SLOT_OFFSETS: [u32; 2] = [0x0001_0000, 0x0020_0000];

/// Both slots are the same size on purpose — see `docs/ota.md`.
pub const SLOT_SIZE: u32 = 0x001F_0000;

/// Our own bookkeeping sector, inside the range `partitions.csv` reserves as
/// `nvs` and immediately after the three blobs [`crate::config`] keeps there.
pub const STATE_OFFSET: u32 = 0xC000;

/// How many boots an unconfirmed image gets before the selector is pointed back.
///
/// Three rather than one because what this defends against is a bad *image*,
/// not a bad afternoon: a router that is rebooting, or an AP that has not come
/// back yet, should cost a retry and not a rollback.
pub const MAX_ATTEMPTS: u8 = 3;

/// The first byte of every ESP application image. Checked before an image is
/// allowed anywhere near the selector, because the most likely wrong answer
/// from an HTTP server is not a corrupt image but an HTML error page.
pub const IMAGE_MAGIC: u8 = 0xE9;

// The layout above has to agree with `partitions.csv`, and a disagreement would
// show up as a node that flashes an image over its own configuration. Cheaper
// to fail the build.
const _: () = {
    assert!(STATE_OFFSET + SECTOR <= OTADATA_OFFSET);
    assert!(OTADATA_OFFSET + 2 * SECTOR <= SLOT_OFFSETS[0]);
    assert!(SLOT_OFFSETS[0] + SLOT_SIZE <= SLOT_OFFSETS[1]);
    // 4 MB of flash, and nothing may run past the end of it.
    assert!(SLOT_OFFSETS[1] + SLOT_SIZE <= 0x0040_0000);
    // Application partitions must start on a 64 KB boundary.
    assert!(SLOT_OFFSETS[0] % 0x10000 == 0);
    assert!(SLOT_OFFSETS[1] % 0x10000 == 0);
};

// --- otadata -----------------------------------------------------------------

/// One `otadata` entry, i.e. ESP-IDF's `esp_ota_select_entry_t`.
///
/// 32 bytes: the sequence number, a 20-byte label nothing here uses, the image
/// state, and a CRC over the sequence number alone. The label and state are
/// carried so that a selector written by this firmware reads back the way the
/// bootloader — and anyone dumping the sector — expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectEntry {
    pub seq: u32,
    pub state: ImageState,
}

/// The `ota_state` field. Only [`Undefined`](ImageState::Undefined) and
/// [`Valid`](ImageState::Valid) are ever written by this firmware — the
/// pending/aborted states exist for a bootloader-side rollback we deliberately
/// do not rely on — but all of them are recognised so a selector written by
/// ESP-IDF tooling reads back intact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageState {
    New,
    PendingVerify,
    Valid,
    Invalid,
    Aborted,
    Undefined,
}

impl ImageState {
    const fn to_u32(self) -> u32 {
        match self {
            Self::New => 0,
            Self::PendingVerify => 1,
            Self::Valid => 2,
            Self::Invalid => 3,
            Self::Aborted => 4,
            Self::Undefined => 0xFFFF_FFFF,
        }
    }

    const fn from_u32(v: u32) -> Self {
        match v {
            0 => Self::New,
            1 => Self::PendingVerify,
            2 => Self::Valid,
            3 => Self::Invalid,
            4 => Self::Aborted,
            _ => Self::Undefined,
        }
    }
}

/// Serialised length of a [`SelectEntry`].
pub const ENTRY_LEN: usize = 32;

impl SelectEntry {
    pub fn to_bytes(self) -> [u8; ENTRY_LEN] {
        let mut b = [0u8; ENTRY_LEN];
        b[0..4].copy_from_slice(&self.seq.to_le_bytes());
        // b[4..24] is the label, left zeroed.
        b[24..28].copy_from_slice(&self.state.to_u32().to_le_bytes());
        b[28..32].copy_from_slice(&entry_crc(self.seq).to_le_bytes());
        b
    }

    /// Read an entry back, or `None` if it is blank, half-written or corrupt.
    ///
    /// The sequence numbers `0` and `0xFFFF_FFFF` are refused along with a bad
    /// CRC: zero is what an erased-then-zeroed sector reads as, all-ones is what
    /// an erased one reads as, and the bootloader treats neither as a choice.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < ENTRY_LEN {
            return None;
        }
        let seq = u32::from_le_bytes(b[0..4].try_into().ok()?);
        let crc = u32::from_le_bytes(b[28..32].try_into().ok()?);
        if seq == 0 || seq == 0xFFFF_FFFF || crc != entry_crc(seq) {
            return None;
        }
        Some(Self {
            seq,
            state: ImageState::from_u32(u32::from_le_bytes(b[24..28].try_into().ok()?)),
        })
    }
}

/// The CRC the bootloader checks an entry against: `esp_rom_crc32_le` over the
/// four bytes of the sequence number, and nothing else.
///
/// `esp_rom_crc32_le(crc, …)` is the usual reflected CRC-32 with the running
/// value inverted on the way in and out, so seeding it with `u32::MAX` — which
/// is what `bootloader_common_ota_select_crc` does — starts the register at
/// zero rather than at all-ones. That is *not* the same function as
/// [`crate::config`]'s `crc32`, and using that one here would produce a
/// selector the bootloader ignores.
pub const fn entry_crc(seq: u32) -> u32 {
    let bytes = seq.to_le_bytes();
    let mut reg: u32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        reg ^= bytes[i] as u32;
        let mut j = 0;
        while j < 8 {
            let mask = (reg & 1).wrapping_neg();
            reg = (reg >> 1) ^ (0xEDB8_8320 & mask);
            j += 1;
        }
        i += 1;
    }
    !reg
}

/// Which slot the bootloader will run, given both selector entries.
///
/// The rule is the bootloader's: the valid entry with the higher sequence
/// number wins, and the slot is `(seq - 1) % 2`. With neither entry valid there
/// is nothing to choose from and the first application partition runs — which
/// is where a cabled `espflash flash` puts an image, so a board that has never
/// had an OTA update behaves exactly as it does today.
pub fn active(entries: [Option<SelectEntry>; 2]) -> Active {
    let winner = match entries {
        [Some(a), Some(b)] => {
            if a.seq >= b.seq {
                Some((0usize, a))
            } else {
                Some((1usize, b))
            }
        }
        [Some(a), None] => Some((0usize, a)),
        [None, Some(b)] => Some((1usize, b)),
        [None, None] => None,
    };

    match winner {
        Some((entry_index, entry)) => Active {
            slot: slot_for_seq(entry.seq),
            seq: entry.seq,
            entry_index,
            selected: true,
        },
        None => Active {
            slot: 0,
            seq: 0,
            entry_index: 1,
            selected: false,
        },
    }
}

/// What [`active`] worked out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Active {
    /// The slot that is running, as an index into [`SLOT_OFFSETS`].
    pub slot: usize,
    /// The winning sequence number, or `0` when nothing is selected yet.
    pub seq: u32,
    /// Which of the two selector entries won, so the *other* is the one to
    /// write next: a half-written update then leaves the working one untouched.
    pub entry_index: usize,
    /// Whether a selector was found at all, as opposed to falling back.
    pub selected: bool,
}

impl Active {
    /// The slot an update should be written into: the one not running.
    pub const fn target_slot(&self) -> usize {
        1 - self.slot
    }

    /// The selector entry an update should be written into: the one that did
    /// not win, so the running configuration survives a power cut mid-write.
    pub const fn target_entry(&self) -> usize {
        1 - self.entry_index
    }

    /// The sequence number that selects `target_slot` and outranks what is
    /// there now. Sequence numbers only ever go up; the pair `(seq, slot)` is
    /// tied together by `slot = (seq - 1) % 2`, so this steps by one or two.
    pub const fn next_seq(&self) -> u32 {
        let target = self.target_slot();
        let mut seq = self.seq + 1;
        if slot_for_seq(seq) != target {
            seq += 1;
        }
        seq
    }
}

/// The slot a sequence number names. Sequence numbers start at 1.
pub const fn slot_for_seq(seq: u32) -> usize {
    ((seq - 1) % 2) as usize
}

// --- The unconfirmed-image bookkeeping ---------------------------------------

const STATE_MAGIC: u32 = 0x4154_4F53; // "SOTA", little-endian
const STATE_VERSION: u8 = 1;
/// magic(4) + version(1) + attempts(1) + pad(2) + seq(4) + crc(4).
pub const STATE_LEN: usize = 16;

/// What the node remembers across a reboot about an update it has not yet
/// decided about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pending {
    /// The selector sequence number this is about, so a stale record cannot be
    /// mistaken for one describing the image that is actually running.
    pub seq: u32,
    /// Boots so far on this image without a completed publish round.
    pub attempts: u8,
}

impl Pending {
    pub fn to_bytes(self) -> [u8; STATE_LEN] {
        let mut b = [0u8; STATE_LEN];
        b[0..4].copy_from_slice(&STATE_MAGIC.to_le_bytes());
        b[4] = STATE_VERSION;
        b[5] = self.attempts;
        b[8..12].copy_from_slice(&self.seq.to_le_bytes());
        let crc = crate::config::crc32(&b[0..12]);
        b[12..16].copy_from_slice(&crc.to_le_bytes());
        b
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < STATE_LEN {
            return None;
        }
        if u32::from_le_bytes(b[0..4].try_into().ok()?) != STATE_MAGIC || b[4] != STATE_VERSION {
            return None;
        }
        if u32::from_le_bytes(b[12..16].try_into().ok()?) != crate::config::crc32(&b[0..12]) {
            return None;
        }
        Some(Self {
            seq: u32::from_le_bytes(b[8..12].try_into().ok()?),
            attempts: b[5],
        })
    }
}

/// What to do about the image that is running, asked once per publish attempt.
///
/// **Per attempt, not per boot**, and the difference is the whole reason this
/// is not the obvious design. A battery node cold-boots out of deep sleep every
/// few seconds and only brings the radio up on its heartbeat, so counting boots
/// would exhaust three attempts inside a minute and roll back an image that had
/// never once been given the chance to publish. Counting attempts to reach the
/// broker gives `terrasse` three heartbeats — half an hour — and a mains node
/// three rounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to decide: no update is outstanding, or the record does not
    /// describe what is running.
    Nothing,
    /// An unconfirmed image, on its `attempts`-th try. Carry on and see whether
    /// this round reaches the broker.
    Trying { attempts: u8 },
    /// It has had its chances. Point the selector back and reset.
    RollBack,
}

/// Decide what this publish attempt means, given the stored record and the
/// running selector.
///
/// Separated from the flash access so the rule itself — which is the part that
/// decides whether a node comes back — is exercised on the host.
pub fn on_attempt(stored: Option<Pending>, running_seq: u32, max_attempts: u8) -> Verdict {
    match stored {
        // A record about some other sequence number is stale: it survived a
        // cabled reflash, or a rollback has already happened. Not ours.
        Some(p) if p.seq != running_seq => Verdict::Nothing,
        Some(p) => {
            let attempts = p.attempts.saturating_add(1);
            if attempts > max_attempts {
                Verdict::RollBack
            } else {
                Verdict::Trying { attempts }
            }
        }
        None => Verdict::Nothing,
    }
}

// --- The offer on the broker -------------------------------------------------

/// An update offer, as it arrives retained on `smarthome/<node>/ota`.
///
/// ```text
/// {"version":"kueche-425e2c4","url":"http://192.168.1.67/fw/kueche-425e2c4.bin",
///  "sha256":"…64 hex…","size":738528}
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer<'a> {
    pub version: &'a str,
    pub url: &'a str,
    pub sha256: [u8; 32],
    pub size: u32,
}

/// Parse an offer, refusing anything that is not complete and plausible.
///
/// Hand-rolled rather than pulled from a JSON crate for the same reason the
/// rest of this firmware's parsing is: four fields, a fixed shape, and a
/// payload that arrives from the broker and must not be able to panic a node.
/// Every refusal below is a message that would otherwise start a download that
/// cannot finish.
pub fn parse_offer<'a>(
    payload: &'a str,
    current_version: &str,
    node_id: &str,
) -> Result<Offer<'a>, OfferError> {
    let version = field(payload, "version").ok_or(OfferError::Incomplete)?;
    let url = field(payload, "url").ok_or(OfferError::Incomplete)?;
    let digest_text = field(payload, "sha256").ok_or(OfferError::Incomplete)?;
    let size_text = field(payload, "size").ok_or(OfferError::Incomplete)?;

    if version == current_version {
        return Err(OfferError::AlreadyRunning);
    }
    if !is_for_node(version, node_id) {
        return Err(OfferError::WrongNode);
    }
    let sha256 = crate::sha256::parse_hex_digest(digest_text).ok_or(OfferError::BadDigest)?;
    let size: u32 = size_text.parse().map_err(|_| OfferError::BadSize)?;
    // An image that cannot fit is refused here rather than three quarters of
    // the way through writing a slot.
    if size == 0 || size > SLOT_SIZE {
        return Err(OfferError::BadSize);
    }
    Ok(Offer {
        version,
        url,
        sha256,
        size,
    })
}

/// Does this version string name an image built for `node_id`?
///
/// The check exists because of what happened on 2026-09-17: a `wohnzimmer`
/// image on the `terrasse` board, eleven hours of readings under the wrong
/// name, and nothing between the shell and the flash that could have noticed.
/// Over the air there can be: the version string carries the node it was built
/// for, and a node refuses anything that is not its own.
pub fn is_for_node(version: &str, node_id: &str) -> bool {
    version
        .strip_prefix(node_id)
        .is_some_and(|rest| rest.starts_with('-'))
}

/// Why an offer was not acted on. Logged rather than published: the node has no
/// obligation to explain itself to the broker, and every one of these is
/// something a human typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferError {
    Incomplete,
    AlreadyRunning,
    /// An image built for another node, which is the mistake this whole check
    /// exists to make impossible over the air.
    WrongNode,
    BadDigest,
    BadSize,
}

impl OfferError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Incomplete => "offer is missing a field",
            Self::AlreadyRunning => "offer names the version already running",
            Self::WrongNode => "offer names an image built for another node",
            Self::BadDigest => "offer's sha256 is not 64 hex characters",
            Self::BadSize => "offer's size is zero, unparseable or larger than a slot",
        }
    }
}

/// Pull one field's value out of a flat JSON object. Strings come back without
/// their quotes, numbers as their digits; nothing here handles nesting or
/// escapes, and nothing in an offer needs it.
fn field<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let mut needle = heapless::String::<24>::new();
    needle.push('"').ok()?;
    needle.push_str(key).ok()?;
    needle.push_str("\":").ok()?;

    // Tolerate a space after the colon without tolerating a different key that
    // merely ends the same way: the needle includes the opening quote.
    let start = json.find(needle.as_str())? + needle.len();
    let rest = json[start..].trim_start();
    if let Some(quoted) = rest.strip_prefix('"') {
        let end = quoted.find('"')?;
        Some(&quoted[..end])
    } else {
        let end = rest.find([',', '}']).unwrap_or(rest.len());
        let value = rest[..end].trim();
        (!value.is_empty()).then_some(value)
    }
}

// --- Writing an image (device only) ------------------------------------------

/// Writes an incoming image into the inactive slot, hashing it on the way past.
///
/// The digest is computed as the bytes arrive rather than by reading the slot
/// back afterwards: it costs nothing extra, and a read-back would only prove
/// that flash stored what flash was given.
///
/// **It stages a whole sector before writing one.** `esp-storage` implements a
/// write as read-erase-write of every sector it touches, so handing it the
/// 200-byte pieces a TCP stream arrives in would erase the same sector a dozen
/// times over — twelve times the wear, and twelve times as long spent with the
/// cache disabled, which is the part of this that runs while the radio is up.
/// The staging buffer is borrowed rather than owned so the caller decides where
/// 4 KB comes from; `main` takes it from a `StaticCell` once at boot rather
/// than putting it on a task stack.
#[cfg(feature = "hal")]
pub struct Writer<'a> {
    slot: usize,
    expected: u32,
    /// Bytes accepted from the network, staged or not.
    taken: u32,
    /// Bytes actually in flash.
    flushed: u32,
    buf: &'a mut [u8],
    filled: usize,
    hasher: crate::sha256::Sha256,
    flash: FlashStorage,
}

#[cfg(feature = "hal")]
impl<'a> Writer<'a> {
    /// Start writing `expected` bytes into `slot`, staging through `buf`.
    ///
    /// `buf` must be a whole number of flash sectors, because that is what
    /// makes every flush land on a sector boundary — the one property that
    /// keeps this from erasing a sector more than once.
    pub fn new(slot: usize, expected: u32, buf: &'a mut [u8]) -> Result<Self, &'static str> {
        if slot >= SLOT_OFFSETS.len() {
            return Err("no such slot");
        }
        if expected == 0 || expected > SLOT_SIZE {
            return Err("image does not fit the slot");
        }
        if buf.is_empty() || buf.len() % SECTOR as usize != 0 {
            return Err("staging buffer must be a whole number of sectors");
        }
        Ok(Self {
            slot,
            expected,
            taken: 0,
            flushed: 0,
            buf,
            filled: 0,
            hasher: crate::sha256::Sha256::new(),
            flash: FlashStorage::new(),
        })
    }

    /// How far along this is, for the log line that tells someone watching that
    /// it has not simply hung.
    pub const fn progress(&self) -> (u32, u32) {
        (self.taken, self.expected)
    }

    /// Take the next bytes of the image.
    ///
    /// The first byte is checked for the image magic before anything is
    /// written, because the likeliest wrong answer from an HTTP server is not a
    /// corrupt image but a perfectly well-formed error page.
    pub fn write(&mut self, mut chunk: &[u8]) -> Result<(), &'static str> {
        if chunk.is_empty() {
            return Ok(());
        }
        if self.taken == 0 && chunk[0] != IMAGE_MAGIC {
            return Err("not an ESP image (wrong magic byte)");
        }
        if self.taken + chunk.len() as u32 > self.expected {
            return Err("server sent more than the offer promised");
        }
        self.hasher.update(chunk);
        self.taken += chunk.len() as u32;

        while !chunk.is_empty() {
            let take = (self.buf.len() - self.filled).min(chunk.len());
            self.buf[self.filled..self.filled + take].copy_from_slice(&chunk[..take]);
            self.filled += take;
            chunk = &chunk[take..];
            if self.filled == self.buf.len() {
                self.flush()?;
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), &'static str> {
        if self.filled == 0 {
            return Ok(());
        }
        self.flash
            .write(
                SLOT_OFFSETS[self.slot] + self.flushed,
                &self.buf[..self.filled],
            )
            .map_err(|_| "flash write failed")?;
        self.flushed += self.filled as u32;
        self.filled = 0;
        Ok(())
    }

    /// Check the finished image against the offer's digest. Consumes the writer
    /// so a failed image cannot be activated by a later call.
    pub fn finish(mut self, expected_digest: &[u8; 32]) -> Result<usize, &'static str> {
        if self.taken != self.expected {
            return Err("image is shorter than the offer promised");
        }
        self.flush()?;
        if &self.hasher.finish() != expected_digest {
            return Err("image does not match the offer\'s sha256");
        }
        Ok(self.slot)
    }
}

/// Read both selector entries out of `otadata`.
#[cfg(feature = "hal")]
pub fn read_otadata() -> [Option<SelectEntry>; 2] {
    let mut flash = FlashStorage::new();
    let mut out = [None, None];
    for (i, slot) in out.iter_mut().enumerate() {
        let mut buf = [0u8; ENTRY_LEN];
        *slot = flash
            .read(OTADATA_OFFSET + i as u32 * SECTOR, &mut buf)
            .ok()
            .and_then(|()| SelectEntry::from_bytes(&buf));
    }
    out
}

/// Point the selector at `slot`, and record that the image there has not yet
/// proved itself.
///
/// Order matters and is the whole reason this is one function: the pending
/// record is written *before* the selector, so a power cut between the two
/// leaves a node that is still running the old image with a harmless record
/// about a sequence number that is not running — which [`on_boot`] discards.
/// The other order would give a booting-but-broken image no record at all, and
/// therefore no rollback.
#[cfg(feature = "hal")]
pub fn activate(current: Active, slot: usize) -> Result<u32, &'static str> {
    if slot != current.target_slot() {
        return Err("refusing to activate the slot that is already running");
    }
    let seq = current.next_seq();
    store_pending(Some(Pending { seq, attempts: 0 }))?;

    let entry = SelectEntry {
        seq,
        state: ImageState::Undefined,
    };
    FlashStorage::new()
        .write(
            OTADATA_OFFSET + current.target_entry() as u32 * SECTOR,
            &entry.to_bytes(),
        )
        .map_err(|_| "otadata write failed")?;
    Ok(seq)
}

/// Point the selector back at the slot that was running before `unwanted`, used
/// when an image has failed to publish often enough to be given up on.
///
/// This writes a *new, higher* sequence number naming the old slot rather than
/// erasing anything: sequence numbers only go up, and an erase would hand the
/// choice back to a bootloader that would pick `ota_0` regardless of which slot
/// actually works.
#[cfg(feature = "hal")]
pub fn roll_back(current: Active) -> Result<u32, &'static str> {
    let seq = current.next_seq();
    let entry = SelectEntry {
        seq,
        state: ImageState::Undefined,
    };
    FlashStorage::new()
        .write(
            OTADATA_OFFSET + current.target_entry() as u32 * SECTOR,
            &entry.to_bytes(),
        )
        .map_err(|_| "otadata write failed")?;
    clear_pending()?;
    Ok(seq)
}

#[cfg(feature = "hal")]
pub fn load_pending() -> Option<Pending> {
    let mut buf = [0u8; STATE_LEN];
    FlashStorage::new()
        .read(STATE_OFFSET, &mut buf)
        .ok()
        .and_then(|()| Pending::from_bytes(&buf))
}

#[cfg(feature = "hal")]
pub fn store_pending(pending: Option<Pending>) -> Result<(), &'static str> {
    let bytes = match pending {
        Some(p) => p.to_bytes(),
        None => [0u8; STATE_LEN],
    };
    FlashStorage::new()
        .write(STATE_OFFSET, &bytes)
        .map_err(|_| "ota state write failed")
}

#[cfg(feature = "hal")]
pub fn clear_pending() -> Result<(), &'static str> {
    store_pending(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(seq: u32) -> SelectEntry {
        SelectEntry {
            seq,
            state: ImageState::Undefined,
        }
    }

    #[test]
    fn the_crc_is_the_bootloaders_one() {
        // Pinned values, so a "tidy-up" that swaps this for `config::crc32`
        // fails here rather than on a board that ignores its own selector.
        assert_eq!(entry_crc(1), 0x4743_989A);
        assert_eq!(entry_crc(2), 0x55F6_3774);
        assert_eq!(entry_crc(3), 0xED4A_5011);
        assert_eq!(entry_crc(4), 0x709D_68A8);
        // And it is emphatically *not* the CRC the config blobs use.
        assert_ne!(entry_crc(1), crate::config::crc32(&1u32.to_le_bytes()));
    }

    #[test]
    fn the_constants_match_partitions_csv() {
        // Two sources of truth, and a disagreement between them would flash an
        // image over the calibration or select a slot the bootloader does not
        // have. The file is read at compile time, so this costs nothing at run
        // time and cannot go stale.
        let csv = include_str!("../partitions.csv");
        let number = |text: &str| -> u32 {
            match text.strip_prefix("0x") {
                Some(hex) => u32::from_str_radix(hex, 16).unwrap(),
                None => text.parse().unwrap(),
            }
        };

        let mut checked = 0;
        for line in csv.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            let (name, offset, size) = (fields[0], number(fields[3]), number(fields[4]));
            match name {
                "nvs" => {
                    // The bookkeeping sector has to live inside it, next to the
                    // config blobs -- that is what keeps a reflash from wiping
                    // the record of an update on trial.
                    assert!(offset <= STATE_OFFSET);
                    assert!(STATE_OFFSET + SECTOR <= offset + size);
                    checked += 1;
                }
                "otadata" => {
                    assert_eq!(offset, OTADATA_OFFSET);
                    assert_eq!(size, 2 * SECTOR, "two copies, one sector each");
                    checked += 1;
                }
                "ota_0" => {
                    assert_eq!(offset, SLOT_OFFSETS[0]);
                    assert_eq!(size, SLOT_SIZE);
                    checked += 1;
                }
                "ota_1" => {
                    assert_eq!(offset, SLOT_OFFSETS[1]);
                    assert_eq!(size, SLOT_SIZE, "the slots must be the same size");
                    checked += 1;
                }
                _ => {}
            }
        }
        assert_eq!(checked, 4, "partitions.csv is missing a partition");
    }

    #[test]
    fn the_config_blobs_are_not_inside_a_slot() {
        // The sectors `config.rs` writes: identity, calibration, credentials.
        // An image slot starting below them would be written over on the first
        // update, which is the one mistake in here that cannot be undone from
        // the sofa.
        for blob in [0x9000u32, 0xA000, 0xB000, STATE_OFFSET] {
            for (i, start) in SLOT_OFFSETS.iter().enumerate() {
                assert!(
                    blob < *start || blob >= start + SLOT_SIZE,
                    "blob at {blob:#x} is inside slot {i}"
                );
            }
        }
    }

    #[test]
    fn entries_round_trip() {
        for seq in [1u32, 2, 3, 1000, 0xFFFF_FFFE] {
            let e = entry(seq);
            assert_eq!(SelectEntry::from_bytes(&e.to_bytes()), Some(e));
        }
    }

    #[test]
    fn a_blank_or_corrupt_entry_is_not_a_choice() {
        // An erased sector, a zeroed one, and a good entry with one bit flipped.
        assert_eq!(SelectEntry::from_bytes(&[0xFF; ENTRY_LEN]), None);
        assert_eq!(SelectEntry::from_bytes(&[0x00; ENTRY_LEN]), None);
        let mut b = entry(7).to_bytes();
        b[0] ^= 0x01;
        assert_eq!(SelectEntry::from_bytes(&b), None);
        // Too short to be an entry at all.
        assert_eq!(SelectEntry::from_bytes(&[0u8; 8]), None);
    }

    #[test]
    fn a_board_that_has_never_been_updated_runs_slot_zero() {
        let a = active([None, None]);
        assert_eq!(a.slot, 0);
        assert!(!a.selected);
        // And the first update must go to the other slot.
        assert_eq!(a.target_slot(), 1);
        assert_eq!(slot_for_seq(a.next_seq()), 1);
    }

    #[test]
    fn the_higher_sequence_number_wins() {
        assert_eq!(active([Some(entry(1)), Some(entry(2))]).slot, 1);
        assert_eq!(active([Some(entry(3)), Some(entry(2))]).slot, 0);
        // One valid entry is enough.
        assert_eq!(active([Some(entry(2)), None]).slot, 1);
        assert_eq!(active([None, Some(entry(1))]).slot, 0);
    }

    #[test]
    fn the_entry_written_next_is_never_the_winning_one() {
        // The property that makes a half-finished update harmless.
        for (a, b) in [(1u32, 2u32), (2, 1), (5, 6)] {
            let act = active([Some(entry(a)), Some(entry(b))]);
            assert_ne!(act.target_entry(), act.entry_index);
        }
        assert_ne!(active([Some(entry(1)), None]).target_entry(), 0);
    }

    #[test]
    fn sequence_numbers_climb_and_name_the_target_slot() {
        // Walk an update chain and check both invariants at every step: the
        // number goes up, and it names the slot that is not running.
        let mut entries = [None, None];
        let mut previous = 0;
        for _ in 0..8 {
            let act = active(entries);
            let seq = act.next_seq();
            assert!(seq > previous, "{seq} must outrank {previous}");
            assert_eq!(slot_for_seq(seq), act.target_slot());
            entries[act.target_entry()] = Some(entry(seq));
            previous = seq;
            // The new selector must actually select what we just aimed at.
            assert_eq!(active(entries).slot, act.target_slot());
        }
    }

    #[test]
    fn pending_records_round_trip_and_reject_rubbish() {
        let p = Pending {
            seq: 42,
            attempts: 2,
        };
        assert_eq!(Pending::from_bytes(&p.to_bytes()), Some(p));
        assert_eq!(Pending::from_bytes(&[0xFF; STATE_LEN]), None);
        assert_eq!(Pending::from_bytes(&[0x00; STATE_LEN]), None);
        let mut b = p.to_bytes();
        b[5] ^= 0xFF; // attempts, which is covered by the CRC
        assert_eq!(Pending::from_bytes(&b), None);
    }

    #[test]
    fn an_image_gets_three_attempts_to_prove_itself() {
        let seq = 9;
        let mut attempts = 0;
        for boot in 1..=MAX_ATTEMPTS {
            match on_attempt(Some(Pending { seq, attempts }), seq, MAX_ATTEMPTS) {
                Verdict::Trying { attempts: a } => {
                    assert_eq!(a, boot);
                    attempts = a;
                }
                other => panic!("boot {boot} decided {other:?}"),
            }
        }
        assert_eq!(
            on_attempt(Some(Pending { seq, attempts }), seq, MAX_ATTEMPTS),
            Verdict::RollBack
        );
    }

    #[test]
    fn a_record_about_another_image_is_ignored() {
        // What a cabled reflash leaves behind: a record naming a sequence
        // number that is no longer running. Rolling back on that would undo a
        // deliberate flash.
        assert_eq!(
            on_attempt(Some(Pending { seq: 4, attempts: 9 }), 7, MAX_ATTEMPTS),
            Verdict::Nothing
        );
        assert_eq!(on_attempt(None, 7, MAX_ATTEMPTS), Verdict::Nothing);
    }

    const GOOD_DIGEST: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn offer_json(version: &str, size: &str) -> heapless::String<256> {
        let mut s = heapless::String::new();
        core::fmt::Write::write_fmt(
            &mut s,
            format_args!(
                r#"{{"version":"{version}","url":"http://192.168.1.67/fw/x.bin","sha256":"{GOOD_DIGEST}","size":{size}}}"#
            ),
        )
        .unwrap();
        s
    }

    #[test]
    fn a_well_formed_offer_parses() {
        let json = offer_json("kueche-425e2c4", "738528");
        let offer = parse_offer(&json, "kueche-2a9168c", "kueche").unwrap();
        assert_eq!(offer.version, "kueche-425e2c4");
        assert_eq!(offer.url, "http://192.168.1.67/fw/x.bin");
        assert_eq!(offer.size, 738_528);
        assert_eq!(offer.sha256[0], 0xBA);
    }

    #[test]
    fn the_version_already_running_is_not_an_update() {
        // The retained offer is re-delivered on every connect, so this is the
        // *common* case, not an error case: without it every reconnect would
        // start a download and a reboot loop.
        let json = offer_json("kueche-425e2c4", "738528");
        assert_eq!(
            parse_offer(&json, "kueche-425e2c4", "kueche"),
            Err(OfferError::AlreadyRunning)
        );
    }

    #[test]
    fn offers_that_cannot_be_acted_on_are_refused() {
        // An image larger than the slot, and one of zero length.
        assert_eq!(
            parse_offer(&offer_json("kueche-v2", "2031617"), "kueche-v1", "kueche"),
            Err(OfferError::BadSize)
        );
        assert_eq!(
            parse_offer(&offer_json("kueche-v2", "0"), "kueche-v1", "kueche"),
            Err(OfferError::BadSize)
        );
        assert_eq!(
            parse_offer(&offer_json("kueche-v2", "lots"), "kueche-v1", "kueche"),
            Err(OfferError::BadSize)
        );
        // A truncated digest, which is what a hand-edited offer looks like.
        let short = r#"{"version":"kueche-v2","url":"http://h/f","sha256":"abcd","size":100}"#;
        assert_eq!(
            parse_offer(short, "kueche-v1", "kueche"),
            Err(OfferError::BadDigest)
        );
        // Missing fields, one at a time.
        for json in [
            r#"{"url":"http://h/f","sha256":"x","size":1}"#,
            r#"{"version":"v2","sha256":"x","size":1}"#,
            r#"{"version":"v2","url":"http://h/f","size":1}"#,
            r#"{"version":"v2","url":"http://h/f","sha256":"x"}"#,
        ] {
            assert_eq!(
                parse_offer(json, "kueche-v1", "kueche"),
                Err(OfferError::Incomplete)
            );
        }
        // And the empty payload a `-r -n` leaves behind when an offer is
        // withdrawn, which must be a no-op rather than a panic.
        assert_eq!(
            parse_offer("", "kueche-v1", "kueche"),
            Err(OfferError::Incomplete)
        );
    }

    #[test]
    fn an_image_built_for_another_node_is_refused() {
        // The over-the-air form of the 2026-09-17 mistake: an image for the
        // living room offered to the bathroom node.
        let json = offer_json("wohnzimmer-425e2c4", "738528");
        assert_eq!(
            parse_offer(&json, "bad-2a9168c", "bad"),
            Err(OfferError::WrongNode)
        );
        // And the rule itself, including the cases a plain `starts_with` gets
        // wrong: a node whose name is a prefix of another's.
        assert!(is_for_node("bad-425e2c4", "bad"));
        assert!(!is_for_node("badezimmer-425e2c4", "bad"));
        assert!(!is_for_node("bad", "bad"));
        assert!(!is_for_node("wohnzimmer-425e2c4", "bad"));
    }

    #[test]
    fn fields_survive_whitespace_and_reordering() {
        let json = r#"{ "size": 100 , "sha256": "ABCD" , "url": "http://h/f" , "version": "v2" }"#;
        assert_eq!(field(json, "version"), Some("v2"));
        assert_eq!(field(json, "url"), Some("http://h/f"));
        assert_eq!(field(json, "size"), Some("100"));
        // A key that is merely a suffix of another must not match it.
        assert_eq!(field(r#"{"otherversion":"x"}"#, "version"), None);
    }
}
