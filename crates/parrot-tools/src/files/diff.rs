use std::fmt::Write;

const CONTEXT_LINES: usize = 3;
const MAXIMUM_EDIT_DISTANCE: usize = 1024;

pub struct FileChange<'a> {
    pub path: &'a str,
    pub before: Option<&'a [u8]>,
    pub after: Option<&'a [u8]>,
}

pub fn render(changes: &[FileChange<'_>]) -> String {
    let mut output = String::new();
    for change in changes {
        render_file(&mut output, change);
    }
    output
}

fn render_file(output: &mut String, change: &FileChange<'_>) {
    let before_path = if change.before.is_none() {
        "/dev/null".to_owned()
    } else {
        header_path("a", change.path)
    };
    let after_path = if change.after.is_none() {
        "/dev/null".to_owned()
    } else {
        header_path("b", change.path)
    };
    writeln!(output, "--- {before_path}\n+++ {after_path}").unwrap();

    if contains_nul(change.before) || contains_nul(change.after) {
        output.push_str("Binary files differ\n");
        return;
    }

    let old_lines = lines(change.before);
    let new_lines = lines(change.after);
    write_hunks(output, &shortest_edit_script(&old_lines, &new_lines));
}

fn header_path(prefix: &str, path: &str) -> String {
    if path == ".." || path.starts_with(&format!("..{}", std::path::MAIN_SEPARATOR)) {
        path.to_owned()
    } else {
        format!("{prefix}/{path}")
    }
}

fn contains_nul(data: Option<&[u8]>) -> bool {
    data.is_some_and(|data| data.contains(&0))
}

fn lines(data: Option<&[u8]>) -> Vec<String> {
    let Some(data) = data.filter(|data| !data.is_empty()) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(data)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut lines: Vec<_> = text.split('\n').map(str::to_owned).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

struct DiffEdit<'a> {
    kind: char,
    text: &'a str,
    old_line: usize,
    new_line: usize,
}

fn shortest_edit_script<'a>(old_lines: &'a [String], new_lines: &'a [String]) -> Vec<DiffEdit<'a>> {
    let maximum = (old_lines.len() + new_lines.len()).min(MAXIMUM_EDIT_DISTANCE);
    let offset = maximum as isize + 1;
    let mut furthest = vec![0isize; 2 * maximum + 3];
    furthest[(offset + 1) as usize] = 0;
    let mut trace = Vec::with_capacity(maximum + 1);

    for distance in 0..=maximum {
        trace.push(furthest.clone());
        let distance = distance as isize;
        for diagonal in (-distance..=distance).step_by(2) {
            let index = (offset + diagonal) as usize;
            let mut x = if diagonal == -distance
                || (diagonal != distance && furthest[index - 1] < furthest[index + 1])
            {
                furthest[index + 1]
            } else {
                furthest[index - 1] + 1
            };
            let mut y = x - diagonal;
            while x >= 0
                && y >= 0
                && x < old_lines.len() as isize
                && y < new_lines.len() as isize
                && old_lines[x as usize] == new_lines[y as usize]
            {
                x += 1;
                y += 1;
            }
            furthest[index] = x;
            if x >= old_lines.len() as isize && y >= new_lines.len() as isize {
                return backtrack_edits(old_lines, new_lines, &trace, distance, offset);
            }
        }
    }

    fallback_edit_script(old_lines, new_lines)
}

fn fallback_edit_script<'a>(old_lines: &'a [String], new_lines: &'a [String]) -> Vec<DiffEdit<'a>> {
    let mut prefix = 0;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old_lines.len() - prefix
        && suffix < new_lines.len() - prefix
        && old_lines[old_lines.len() - suffix - 1] == new_lines[new_lines.len() - suffix - 1]
    {
        suffix += 1;
    }

    let mut edits = Vec::with_capacity(old_lines.len() + new_lines.len());
    for (index, text) in old_lines.iter().enumerate().take(prefix) {
        edits.push(DiffEdit {
            kind: ' ',
            text,
            old_line: index + 1,
            new_line: index + 1,
        });
    }
    for (index, text) in old_lines
        .iter()
        .enumerate()
        .take(old_lines.len() - suffix)
        .skip(prefix)
    {
        edits.push(DiffEdit {
            kind: '-',
            text,
            old_line: index + 1,
            new_line: prefix + 1,
        });
    }
    for (index, text) in new_lines
        .iter()
        .enumerate()
        .take(new_lines.len() - suffix)
        .skip(prefix)
    {
        edits.push(DiffEdit {
            kind: '+',
            text,
            old_line: old_lines.len() - suffix + 1,
            new_line: index + 1,
        });
    }
    for index in 0..suffix {
        let old_index = old_lines.len() - suffix + index;
        let new_index = new_lines.len() - suffix + index;
        edits.push(DiffEdit {
            kind: ' ',
            text: &old_lines[old_index],
            old_line: old_index + 1,
            new_line: new_index + 1,
        });
    }
    edits
}

fn backtrack_edits<'a>(
    old_lines: &'a [String],
    new_lines: &'a [String],
    trace: &[Vec<isize>],
    distance: isize,
    offset: isize,
) -> Vec<DiffEdit<'a>> {
    let mut x = old_lines.len();
    let mut y = new_lines.len();
    let mut reversed = Vec::with_capacity(x + y);
    for current in (1..=distance).rev() {
        let furthest = &trace[current as usize];
        let diagonal = x as isize - y as isize;
        let previous_diagonal = if diagonal == -current
            || (diagonal != current
                && furthest[(offset + diagonal - 1) as usize]
                    < furthest[(offset + diagonal + 1) as usize])
        {
            diagonal + 1
        } else {
            diagonal - 1
        };
        let previous_x = furthest[(offset + previous_diagonal) as usize];
        let previous_y = previous_x - previous_diagonal;
        while x > previous_x as usize && y > previous_y as usize {
            x -= 1;
            y -= 1;
            reversed.push(DiffEdit {
                kind: ' ',
                text: &old_lines[x],
                old_line: x + 1,
                new_line: y + 1,
            });
        }
        if x == previous_x as usize {
            y -= 1;
            reversed.push(DiffEdit {
                kind: '+',
                text: &new_lines[y],
                old_line: x + 1,
                new_line: y + 1,
            });
        } else {
            x -= 1;
            reversed.push(DiffEdit {
                kind: '-',
                text: &old_lines[x],
                old_line: x + 1,
                new_line: y + 1,
            });
        }
    }
    while x > 0 && y > 0 {
        x -= 1;
        y -= 1;
        reversed.push(DiffEdit {
            kind: ' ',
            text: &old_lines[x],
            old_line: x + 1,
            new_line: y + 1,
        });
    }
    reversed.reverse();
    reversed
}

fn write_hunks(output: &mut String, edits: &[DiffEdit<'_>]) {
    let mut index = 0;
    while index < edits.len() {
        while index < edits.len() && edits[index].kind == ' ' {
            index += 1;
        }
        if index == edits.len() {
            return;
        }
        let start = index.saturating_sub(CONTEXT_LINES);
        let mut last_change = index;
        for (next, edit) in edits.iter().enumerate().skip(index + 1) {
            if edit.kind == ' ' {
                continue;
            }
            if next - last_change - 1 > CONTEXT_LINES * 2 {
                break;
            }
            last_change = next;
        }
        let end = edits.len().min(last_change + CONTEXT_LINES + 1);
        let old_count = edits[start..end]
            .iter()
            .filter(|edit| edit.kind != '+')
            .count();
        let new_count = edits[start..end]
            .iter()
            .filter(|edit| edit.kind != '-')
            .count();
        let old_start = edits[start].old_line - usize::from(old_count == 0);
        let new_start = edits[start].new_line - usize::from(new_count == 0);
        writeln!(
            output,
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@"
        )
        .unwrap();
        for edit in &edits[start..end] {
            writeln!(output, "{}{}", edit.kind, edit.text).unwrap();
        }
        index = end;
    }
}
