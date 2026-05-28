import re
with open("src/trajectory/export.rs", "r") as f:
    content = f.read()

struct_def = """
#[cfg(feature = "jupyter-export")]
pub struct JupyterExporter;

#[cfg(feature = "jupyter-export")]
impl TrajectoryExporter for JupyterExporter {
    fn export(_trajectory: &Trajectory) -> String {
        String::new()
    }
}
"""
test_def = """
    #[cfg(feature = "jupyter-export")]
    #[test]
    fn test_jupyter_export_format() {
        let mut t = Trajectory::new();
        t.info.task = Some("Add a feature".to_string());
        t.info.outcome = Some("submitted".to_string());

        t.record_message(&Message::user("Hello agent"));
        t.record_message(&Message::user("Hello user"));

        let jupyter = JupyterExporter::export(&t);

        assert!(jupyter.starts_with("{"));
        assert!(jupyter.contains("\\"cells\\""));
        assert!(jupyter.contains("Add a feature"));
        assert!(jupyter.contains("submitted"));
        assert!(jupyter.contains("Hello agent"));
        assert!(jupyter.contains("Hello user"));
    }
}
"""
# Ensure we only replace once by compiling regex and using count=1
content = re.sub(r'pub struct HtmlExporter;', 'pub struct HtmlExporter;\n' + struct_def, content, count=1)
# Replace the very end
content = re.sub(r'\}\s*$', '', content) + test_def
with open("src/trajectory/export.rs", "w") as f:
    f.write(content)
