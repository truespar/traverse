//! CRC32 checksum helpers for pages and headers.

use crate::storage::format::PAGE_HEADER_SIZE;

/// Checksum offset in the page header.
const CHECKSUM_OFFSET: usize = 40;

/// CRC32 over the whole page except the checksum field: header `[0..CHECKSUM_OFFSET]`, header
/// `[CHECKSUM_OFFSET+4..PAGE_HEADER_SIZE]`, then all data.
pub fn compute_page_checksum(page_buf: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&page_buf[..CHECKSUM_OFFSET]);
    hasher.update(&page_buf[CHECKSUM_OFFSET + 4..PAGE_HEADER_SIZE]);
    hasher.update(&page_buf[PAGE_HEADER_SIZE..]);
    hasher.finalize()
}

/// Writes the CRC32 into the page header.
pub fn write_page_checksum(page_buf: &mut [u8]) {
    let crc = compute_page_checksum(page_buf);
    page_buf[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
}

/// Stored checksum from the page header.
pub fn read_page_checksum(page_buf: &[u8]) -> u32 {
    u32::from_le_bytes(
        page_buf[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4]
            .try_into()
            .unwrap(),
    )
}

/// Stored checksum matches the computed one?
pub fn verify_page_checksum(page_buf: &[u8]) -> bool {
    let stored = read_page_checksum(page_buf);
    let computed = compute_page_checksum(page_buf);
    stored == computed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::format::PAGE_SIZE;

    #[test]
    fn write_and_verify_checksum() {
        let mut page = vec![0u8; PAGE_SIZE];
        // some page data
        page[PAGE_HEADER_SIZE] = 0x42;
        page[PAGE_HEADER_SIZE + 1] = 0xFF;

        write_page_checksum(&mut page);
        assert!(verify_page_checksum(&page));
    }

    #[test]
    fn tampered_data_fails_verification() {
        let mut page = vec![0u8; PAGE_SIZE];
        page[PAGE_HEADER_SIZE] = 0x42;

        write_page_checksum(&mut page);
        // tamper with data
        page[PAGE_HEADER_SIZE] = 0x00;
        assert!(!verify_page_checksum(&page));
    }

    #[test]
    fn tampered_header_fails_verification() {
        let mut page = vec![0u8; PAGE_SIZE];
        write_page_checksum(&mut page);
        // tamper with a header byte (not the checksum)
        page[0] = 0xFF;
        assert!(!verify_page_checksum(&page));
    }
}
