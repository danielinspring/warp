use super::*;

#[test]
fn parse_shortstat_reads_insertions_and_deletions() {
    let stats = parse_shortstat(" 1 file changed, 2 insertions(+), 17 deletions(-)\n").unwrap();
    assert_eq!(
        stats,
        ShortStat {
            lines_added: 2,
            lines_removed: 17,
        }
    );
}

#[test]
fn parse_shortstat_returns_none_for_blank_output() {
    assert!(parse_shortstat("").is_none());
    assert!(parse_shortstat("  \n").is_none());
}

#[test]
fn parse_shortstat_defaults_missing_deletions_to_zero() {
    let stats = parse_shortstat(" 3 files changed, 42 insertions(+)").unwrap();
    assert_eq!(
        stats,
        ShortStat {
            lines_added: 42,
            lines_removed: 0,
        }
    );
}

#[test]
fn truncate_on_char_boundary_backs_off_inside_multibyte_char() {
    // "é" occupies bytes 1..3, so a cap of 2 lands mid-character.
    let s = "aé";
    assert_eq!(truncate_on_char_boundary(s, 2), "a");
    assert_eq!(truncate_on_char_boundary(s, 3), "aé");
    assert_eq!(truncate_on_char_boundary(s, 10), "aé");
    assert_eq!(truncate_on_char_boundary(s, 0), "");
}
