//! Text tables: Go's `text/tabwriter` (migration status) and the olekukonko/tablewriter
//! layout the CLI uses for inconsistent objects.

/// `tabwriter.NewWriter(w, minwidth 0, tabwidth 8, padding 2, ' ', 0)`: tab-terminated
/// cells are aligned per column over each block of consecutive lines that have that column.
pub fn tabwrite(text: &str) -> String {
    let lines: Vec<Vec<&str>> = text.split('\n').map(|l| l.split('\t').collect()).collect();
    // widths[line][col] for every tab-terminated cell.
    let mut widths: Vec<Vec<usize>> = lines
        .iter()
        .map(|l| vec![0; l.len().saturating_sub(1)])
        .collect();
    format_block(&lines, &mut widths, 0, 0, lines.len());
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        for (c, cell) in line.iter().enumerate() {
            out.push_str(cell);
            if c + 1 < line.len() {
                let pad = widths[i][c].saturating_sub(cell.chars().count());
                out.push_str(&" ".repeat(pad));
            }
        }
        if i + 1 < lines.len() {
            out.push('\n');
        }
    }
    out
}

fn format_block(
    lines: &[Vec<&str>],
    widths: &mut [Vec<usize>],
    col: usize,
    start: usize,
    end: usize,
) {
    let mut i = start;
    while i < end {
        if lines[i].len().saturating_sub(1) <= col {
            i += 1;
            continue;
        }
        // A block: consecutive lines with a terminated cell in this column.
        let block_start = i;
        let mut width = 0;
        while i < end && lines[i].len().saturating_sub(1) > col {
            width = width.max(lines[i][col].chars().count() + 2);
            i += 1;
        }
        for w in widths.iter_mut().take(i).skip(block_start) {
            w[col] = width;
        }
        format_block(lines, widths, col + 1, block_start, i);
    }
}

/// tablewriter with `BorderNone`, left-aligned cells and an upper-cased header.
pub fn render_table(header: &[&str], rows: &[Vec<String>]) -> String {
    let cols = header.len();
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (c, cell) in row.iter().enumerate().take(cols) {
            widths[c] = widths[c].max(cell.chars().count());
        }
    }
    let line = |cells: Vec<String>| -> String {
        let parts: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(c, cell)| format!(" {cell}{} ", " ".repeat(widths[c] - cell.chars().count())))
            .collect();
        parts.join("│")
    };
    let mut out = String::new();
    out.push_str(&line(header.iter().map(|h| h.to_uppercase()).collect()));
    out.push('\n');
    let sep: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
    out.push_str(&sep.join("┼"));
    out.push('\n');
    for row in rows {
        out.push_str(&line(row.clone()));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn tabwriter_aligns_blocks() {
        let s = "\nDatabase: default\nVERSION\tNAME\tSOURCE STATUS\tDATABASE STATUS\n1\tinit\tPresent\tNot Present\n";
        assert_eq!(
            super::tabwrite(s),
            "\nDatabase: default\nVERSION  NAME  SOURCE STATUS  DATABASE STATUS\n1        init  Present        Not Present\n"
        );
    }
}
