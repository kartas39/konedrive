#!/bin/sh
# The guard of the structure rules (CONTRIBUTING.md, "The structure of the code"):
#
#   1. size: a source file is at most 1,000 lines, a test file 1,500, a Markdown
#      document 1,200;
#   2. no Rust source file holds tests;
#   3. in konedrived, a directory uses only the directories before it in the
#      layer order, and remote/ does not use upload/. Test code is exempt.
#
# It reads the files git tracks, prints one line for each thing against a rule
# and exits with 1 if there is any. Run from anywhere in the checkout:
#
#   scripts/check-structure.sh
#
# What it takes for test code: a file under a `tests` directory, a Rust file
# named `tests.rs`, and a Rust module declared under `#[cfg(test)]` (with
# every file below it). Such a module is test code for the layer order and
# the size limit; a test (`#[test]`) is allowed only in `tests.rs` and under
# `tests/`.
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

function read_rust(file,    line, number, pending, own, top, name, by_path) {
    by_path = test_by_path(file)
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
        if (lines[i] > max) {
            print file ": " lines[i] " lines, " kind " is at most " max " (rule 1)"
            bad++
        }
    }
    # Rule 3 is for source files: what was found in test code is dropped.
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
    print "check-structure: " files " files, nothing against the rules"
}
'
