#!/bin/sh
# The guard of the structure rules (CONTRIBUTING.md, "The structure of the code"):
#
#   1. size, as advice: a source file of more than 1,000 lines, a test file of
#      more than 1,500 and a Markdown document of more than 1,200 are listed,
#      and nothing more;
#   2. no Rust source file holds tests;
#   3. in konedrived, a directory uses only the directories before it in the
#      layer order, and remote/ does not use upload/. Test code is exempt.
#   7. a lock of std::sync is taken through the one function that goes on
#      after a panic (konedrived: panic::lock, read, write), never with
#      `.lock().unwrap()` or a recovery written out. Test code is exempt.
#
# It reads the files git tracks, prints one line for each thing against a rule
# and exits with 1 if there is any. A file over the advised size is printed too,
# and does not change the exit status. Run from anywhere in the checkout:
#
#   scripts/check-structure.sh
#
# What it takes for test code: a file under a `tests` directory, a Rust file
# named `tests.rs`, and a Rust module declared under `#[cfg(test)]` (with
# every file below it). Such a module is test code for the layer order and
# the advised size; a test (`#[test]`) is allowed only in `tests.rs` and under
# `tests/`.
#
# A file under a `generated` directory is written by a generator
# (crates/konedrive-text): the size advice leaves it out.
#
# What it does not see is in docs/limitations/D30.md.
set -eu

cd "$(dirname "$0")/.."

git ls-files -- '*.rs' '*.cpp' '*.h' '*.qml' '*.sh' '*.py' '*.md' | LC_ALL=C awk '
BEGIN {
    SOURCE_MAX = 1000
    TEST_MAX = 1500
    MARKDOWN_MAX = 1200

    # The daemon: its directories in layer order.
    DAEMON = "crates/konedrived/src/"
    count = split("config account helper folder conditions status hydration local upload remote desktop sync daemon dbus", order, " ")
    for (i = 1; i <= count; i++)
        layer[order[i]] = i
}

# A file under a `tests` directory, or a Rust file named for tests.
function test_by_path(file) {
    return file ~ /(^|\/)tests\// || file ~ /(^|\/)tests\.rs$/
}

# Where the modules a Rust file declares are: the file`s directory for
# mod.rs, lib.rs and main.rs, the directory named like the file otherwise.
function children(file,    dir, name) {
    dir = file
    sub(/[^\/]*$/, "", dir)
    name = file
    sub(/^.*\//, "", name)
    sub(/\.rs$/, "", name)
    if (name == "mod" || name == "lib" || name == "main")
        return dir
    return dir name "/"
}

# The daemon`s directory a file is in, or "" (the files at the top, and
# everything outside the daemon).
function area(file,    rest) {
    if (index(file, DAEMON) != 1)
        return ""
    rest = substr(file, length(DAEMON) + 1)
    if (rest !~ /\//)
        return ""
    sub(/\/.*$/, "", rest)
    return rest
}

# Every `crate::name` (and, in an area`s mod.rs, `super::name`) of a line
# that goes against the layer order is kept for the report.
function uses(file, number, line, own, top,    text, name) {
    text = line
    sub(/\/\/.*$/, "", text)
    while (match(text, /(crate|super)::[a-z_][a-z_0-9]*/)) {
        name = substr(text, RSTART, RLENGTH)
        text = substr(text, RSTART + RLENGTH)
        if (name ~ /^super/ && !top)
            continue
        sub(/^(crate|super)::/, "", name)
        if (!(name in layer))
            continue
        if (layer[name] > layer[own] || (own == "remote" && name == "upload")) {
            found++
            found_file[found] = file
            found_text[found] = file ":" number ": " own "/ uses " name "/ (rule 3: the layer order)"
        }
    }
}

# Whether rule 7 is asked of a file: not of the files that hold the one
# function, not of test doubles (`testing.rs`, `testing/`), and not of the helper
# (it has its own `lock`, and root code is not changed for tidiness):
# docs/limitations/D58.md.
function locks_checked(file) {
    if (file == DAEMON "panic.rs" || file == "crates/konedrive-graph/src/lib.rs" || file == "crates/konedrive-tree/src/outbox/changes.rs")
        return 0
    if (file ~ /(^|\/)testing(\.rs$|\/)/)
        return 0
    return index(file, "crates/konedrive-helper/") != 1
}

# A lock, a read or a write taken with nothing passed (or as `Mutex::lock(…)`),
# and then its result unwrapped, dropped or recovered from by hand: on one
# line, or on the next one that is not empty or a comment.
function poison(file, number, line, chained,    text, taken) {
    text = line
    sub(/\/\/.*$/, "", text)
    if (chained && text ~ /^[ \t]*$/)
        return 1
    taken = "(unwrap[a-z_]*|expect|ok|map_err|is_ok|is_err)\\("
    if (text ~ ("(\\.(lock|read|write)\\(\\)|(Mutex|RwLock)::(lock|read|write)\\([^()]*\\))[ \t]*\\." taken) || (chained && text ~ ("^[ \t]*\\." taken))) {
        found++
        found_file[found] = file
        found_text[found] = file ":" number ": a lock taken without panic::lock, read or write (rule 7)"
    }
    return text ~ /(\.(lock|read|write)\(\)|(Mutex|RwLock)::(lock|read|write)\([^()]*\))[ \t]*$/
}

function read_rust(file,    line, number, pending, own, top, name, by_path, locks, chained) {
    by_path = test_by_path(file)
    locks = locks_checked(file)
    chained = 0
    own = area(file)
    top = (file == DAEMON own "/mod.rs")
    if (own != "" && !(own in layer) && !by_path) {
        found++
        found_file[found] = file
        found_text[found] = file ": " own "/ is not in the layer order (rule 3)"
        own = ""
    }
    number = 0
    pending = 0
    while ((getline line < file) > 0) {
        number++
        if (by_path)
            continue
        if (own != "")
            uses(file, number, line, own, top)
        if (locks)
            chained = poison(file, number, line, chained)
        if (line ~ /^[ \t]*#\[(tokio::)?test[]( ]/) {
            found++
            found_file[found] = file
            found_rule[found] = 2
            found_text[found] = file ":" number ": a test outside tests.rs and tests/ (rule 2)"
        }
        if (line ~ /^[ \t]*#\[cfg\(test\)\][ \t]*$/) {
            pending = 1
            continue
        }
        if (!pending)
            continue
        # Other attributes and comments may stand between the two.
        if (line ~ /^[ \t]*(#\[|\/\/)/)
            continue
        pending = 0
        if (line !~ /^[ \t]*(pub(\([a-z]+\))? )?mod [a-z_][a-z_0-9]*[ \t]*[;{]/)
            continue
        if (line ~ /\{/) {
            found++
            found_file[found] = file
            found_rule[found] = 2
            found_text[found] = file ":" number ": a test module in a source file (rule 2)"
            continue
        }
        name = line
        sub(/^.*mod /, "", name)
        sub(/[ \t]*;.*$/, "", name)
        tests++
        test_file[tests] = children(file) name ".rs"
        test_dir[tests] = children(file) name "/"
    }
    close(file)
    return number
}

function read_other(file,    line, number) {
    number = 0
    while ((getline line < file) > 0)
        number++
    close(file)
    return number
}

# Declared under `#[cfg(test)]`, or below such a module.
function test_by_declaration(file,    i) {
    for (i = 1; i <= tests; i++)
        if (file == test_file[i] || index(file, test_dir[i]) == 1)
            return 1
    return 0
}

{
    files++
    name[files] = $0
    lines[files] = ($0 ~ /\.rs$/) ? read_rust($0) : read_other($0)
}

END {
    bad = 0
    for (i = 1; i <= files; i++) {
        file = name[i]
        is_test[file] = test_by_path(file) || (file ~ /\.rs$/ && test_by_declaration(file))
        if (file ~ /\.md$/) {
            max = MARKDOWN_MAX
            kind = "a Markdown document"
        } else if (is_test[file]) {
            max = TEST_MAX
            kind = "a test file"
        } else {
            max = SOURCE_MAX
            kind = "a source file"
        }
        # A generated file (app/generated/) is as long as its source makes it.
        if (lines[i] > max && file !~ /(^|\/)generated\//) {
            print file ": " lines[i] " lines, " kind " is advised to be at most " max " (rule 1, advice)"
            long++
        }
    }
    # Rules 3 and 7 are for source files: what was found in test code is dropped.
    # Rule 2 has no such exemption: a test is in tests.rs or under tests/,
    # which were not read for it at all.
    for (i = 1; i <= found; i++) {
        if (found_rule[i] != 2 && is_test[found_file[i]])
            continue
        print found_text[i]
        bad++
    }
    if (bad) {
        print "check-structure: " bad " against the rules (CONTRIBUTING.md, \"The structure of the code\")"
        exit 1
    }
    print "check-structure: " files " files, nothing against the rules" (long ? "; " long " over the advised size" : "")
}
'
