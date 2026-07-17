use comfy_table::{Cell, Color, Table, Attribute};
fn main() {
    let mut table = Table::new();
    table.set_header([
        Cell::new("Code").add_attribute(Attribute::Bold)
    ]);
    table.add_row([
        Cell::new("123").fg(Color::Cyan)
    ]);
    println!("{table}");
}
