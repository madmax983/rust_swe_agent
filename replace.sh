awk '
BEGIN { in_render_instance_text = 0 }
/^fn render_instance_text/ { in_render_instance_text = 1 }
in_render_instance_text == 1 {
    if ($0 ~ /=== bench inspect ===/) {
        print "    let mut table = Table::new();"
        print "    table"
        print "        .load_preset(UTF8_FULL)"
        print "        .apply_modifier(UTF8_ROUND_CORNERS);"
        print "    table.add_row(vec![\"instance_id\", report.instance_id.as_deref().unwrap_or(\"?\")]);"
    } else {
        print $0
    }
}
in_render_instance_text == 0 { print $0 }
/^}$/ {
    if (in_render_instance_text == 1) {
        in_render_instance_text = 0
    }
}
' src/run/inspect.rs > src/run/inspect_new.rs
