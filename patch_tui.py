import re

with open('src/agent/confirm_tui.rs', 'r') as f:
    content = f.read()

new_test = """    #[test]
    fn test_mouse_scroll_step_from_env() {
        temp_env::with_var("MAXWELL_MOUSE_SCROLL_STEP", None::<&str>, || {
            assert_eq!(mouse_scroll_step_from_env(), DEFAULT_MOUSE_SCROLL_STEP);
        });

        temp_env::with_var("MAXWELL_MOUSE_SCROLL_STEP", Some("7"), || {
            assert_eq!(mouse_scroll_step_from_env(), 7);
        });

        temp_env::with_var("MAXWELL_MOUSE_SCROLL_STEP", Some("not-a-number"), || {
            assert_eq!(mouse_scroll_step_from_env(), DEFAULT_MOUSE_SCROLL_STEP);
        });
        temp_env::with_var("MAXWELL_MOUSE_SCROLL_STEP", Some("0"), || {
            assert_eq!(mouse_scroll_step_from_env(), DEFAULT_MOUSE_SCROLL_STEP);
        });
    }"""

pattern = r'#\[test\]\n\s*fn test_mouse_scroll_step_from_env\(\) \{.*?(?=\n\n\s*// `renderer_loop` uses `handle_mouse`)'
content = re.sub(pattern, new_test, content, flags=re.DOTALL)

with open('src/agent/confirm_tui.rs', 'w') as f:
    f.write(content)
