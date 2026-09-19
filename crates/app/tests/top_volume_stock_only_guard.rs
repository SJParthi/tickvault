//! Pins the 2026-09-18 (FOURTH) operator narrowing: the volume board ranks,
//! sorts and persists **stock options only**.
//!
//! Operator, verbatim: *"Just go ahead with the stocks options alone for top
//! volume dude"*. Full dated record, the decisive finding and the REJECT list:
//! `.claude/rules/project/websocket-connection-scope-lock.md`
//! § "2026-09-18 (FOURTH)".
//!
//! # Why a SOURCE scan and not a behavioural test
//!
//! The thing being pinned is a POLICY — which families are admitted — and its
//! whole enforcement is one list plus three call sites that read it. A
//! behavioural test would have to drive a live `LiveIngest` through a market
//! window to observe an absence, and an absence is exactly what a flaky
//! harness reports for free. The scan asks the checkable question instead:
//! does the list still say Stock only, and does every admission point still
//! read the list rather than its own literal?
//!
//! Every scan below strips comments first. This file's own prose names
//! `OptionFamily::Index` repeatedly, and a raw `contains` over the source
//! would be satisfied by a doc line — the vacuous-guard shape this repository
//! has now recorded nine times.

/// Returns `src` with every `//` line comment removed, so a scan cannot be
/// satisfied (or defeated) by prose.
fn strip_line_comments(src: &str) -> String {
    src.lines()
        .map(|line| match line.find("//") {
            Some(i) => &line[..i],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn feed_stack_code() -> String {
    strip_line_comments(include_str!("../src/dhan_feed_stack.rs"))
}

#[test]
fn the_ranked_family_list_admits_stock_and_nothing_else() {
    let code = feed_stack_code();

    let start = code
        .find("const RANKED_OPTION_FAMILIES")
        .expect("RANKED_OPTION_FAMILIES is the single source of the family policy; it is gone");
    let decl = &code[start..];
    // To `];`, NOT to the first `;` -- the array TYPE carries its own
    // semicolon (`[OptionFamily; 1]`), so a naive `find(';')` stops before the
    // initializer and every assertion below passes on an empty slice. Caught
    // by this file's own `guard_self_test` only after it was fixed to assert
    // the POSITIVE case: the first version of that self-test used the same
    // wrong slice and so proved nothing.
    let end = decl
        .find("];")
        .expect("the RANKED_OPTION_FAMILIES declaration has no `];` terminator");
    let decl = &decl[..=end];

    assert!(
        decl.contains("OptionFamily::Stock"),
        "RANKED_OPTION_FAMILIES must admit the Stock family. Declaration read: {decl}"
    );
    assert!(
        !decl.contains("OptionFamily::Index"),
        "RANKED_OPTION_FAMILIES must NOT admit the Index family -- that is the \
         2026-09-18 (FOURTH) narrowing, and re-admitting it needs a fresh dated \
         quote in websocket-connection-scope-lock.md FIRST. Declaration read: {decl}"
    );
    assert!(
        decl.contains("; 1]"),
        "RANKED_OPTION_FAMILIES must be a ONE-element array. A wider array means a \
         second family was admitted without this guard being re-blessed. \
         Declaration read: {decl}"
    );
}

#[test]
fn no_family_loop_hardcodes_a_family_beside_the_list() {
    let code = feed_stack_code();

    assert!(
        !code.contains("OptionFamily::Index"),
        "an `OptionFamily::Index` reference survives in dhan_feed_stack.rs production \
         code (comments are stripped before this scan). The family policy lives in \
         RANKED_OPTION_FAMILIES and nowhere else -- a literal beside it is how the two \
         drift apart."
    );

    let loops = code.matches("for family in").count();
    assert_eq!(
        loops, 2,
        "expected exactly two family loops (the out-of-window baseline roll and the \
         ranking sweep), found {loops}. A new one must read RANKED_OPTION_FAMILIES too."
    );
    let from_list = code.matches("for family in RANKED_OPTION_FAMILIES").count();
    assert_eq!(
        from_list, 2,
        "all {loops} family loops must iterate RANKED_OPTION_FAMILIES; {from_list} do. \
         A loop over an inline array is a second source of truth."
    );
}

#[test]
fn the_per_tick_path_refuses_a_family_the_list_does_not_admit() {
    let code = feed_stack_code();

    let observe_fn = code
        .find("fn observe_for_ranking")
        .expect("observe_for_ranking is the whole per-tick ranking entry point; it is gone");
    let body = &code[observe_fn..];
    let body_end = body.find("self.leaderboard.observe(").expect(
        "observe_for_ranking no longer calls `leaderboard.observe` -- the per-tick \
                 ranking path changed shape and this guard needs re-blessing",
    );
    let before_observe = &body[..body_end];

    assert!(
        before_observe.contains("RANKED_OPTION_FAMILIES.contains(&owner.family)"),
        "observe_for_ranking must refuse a family the list does not admit BEFORE it \
         reaches `leaderboard.observe`. Without it an index-option tick still pays a \
         hash probe into a board nothing ranks, and the Index map fills for nobody."
    );
}

#[test]
fn the_family_column_and_its_dedup_key_are_untouched() {
    // Deliberately NOT stripped of comments: this scan is about the DDL and the
    // key literal, both of which are code.
    let persistence = include_str!("../../storage/src/top_volume_rank_persistence.rs");

    assert!(
        persistence.contains("ts, tf, family, feed, security_id, segment"),
        "`family` must stay in DEDUP_KEY_TOP_VOLUME_RANK. The schema self-heal is \
         ADD COLUMN IF NOT EXISTS and can NEVER drop a column, so a key that stops \
         naming a live column is a key that no longer matches the table."
    );
    assert!(
        persistence.contains("family SYMBOL"),
        "the `family` column must stay in the CREATE. It becomes constant, which is \
         the point: re-admitting a family is then additive."
    );
}

#[test]
fn the_scope_lock_records_the_narrowing() {
    let lock = include_str!("../../../.claude/rules/project/websocket-connection-scope-lock.md");

    assert!(
        lock.contains("Just go ahead with the stocks options alone for top volume dude"),
        "the operator's verbatim authorization for the stock-only narrowing must stay \
         in websocket-connection-scope-lock.md -- the rule-file-first law makes that \
         record the authority this code answers to"
    );
    assert!(
        lock.contains("2026-09-18 (FOURTH)"),
        "the dated section heading for the stock-only narrowing is missing from \
         websocket-connection-scope-lock.md"
    );
}

/// Bite-proof: the comment stripper must actually strip, or every scan above
/// is satisfiable by prose.
#[test]
fn guard_self_test() {
    let stripped = strip_line_comments("let x = 1; // OptionFamily::Index\nlet y = 2;");
    assert!(
        !stripped.contains("OptionFamily::Index"),
        "the stripper left a line comment intact, so every scan in this file is vacuous"
    );
    assert!(
        stripped.contains("let y = 2;"),
        "the stripper removed live code"
    );

    // And the shape the first test relies on. The naive version of this
    // assertion sliced to the first `;`, which lands INSIDE the array type
    // `[T; 1]` -- so `decl` was `const A: [T`, the negative assertion passed
    // on a slice containing nothing, and the guard proved nothing at all.
    // That is the tenth vacuous-guard shape this repository has recorded, and
    // it appeared in the guard written to record the ninth. The positive
    // assertion is what makes this test able to fail.
    let src = "const A: [T; 1] = [T::Stock];\nconst B: [T; 2] = [T::Index, T::Stock];";
    let decl = &src[src.find("const A").unwrap()..];
    let end = decl.find("];").unwrap();
    let decl = &decl[..=end];
    assert!(
        decl.contains("T::Stock"),
        "the declaration slice stopped short of its own initializer, so every \
         assertion over it is vacuous. Slice read: {decl}"
    );
    assert!(
        !decl.contains("T::Index"),
        "the declaration slice ran past its own terminator and swallowed the next \
         const. Slice read: {decl}"
    );
}
