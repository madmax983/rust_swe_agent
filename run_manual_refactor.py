import re
import os

# 1. Redaction -> stream
# We will use exactly the old `move_redacting.py` since it worked perfectly.
with open('src/redaction.rs', 'r') as f:
    redaction_code = f.read()

def extract_block(code, start_pattern):
    match = re.search(start_pattern, code)
    if not match: return None, code
    start_idx = match.start()
    end_idx = start_idx
    brace_count = 0
    in_block = False
    for i in range(start_idx, len(code)):
        if code[i] == '{':
            brace_count += 1
            in_block = True
        elif code[i] == '}':
            brace_count -= 1
            if in_block and brace_count == 0:
                end_idx = i + 1
                break
    block = code[start_idx:end_idx]
    new_code = code[:start_idx] + code[end_idx:]
    return block, new_code

redact_stream_event_block, redaction_code = extract_block(redaction_code, r'pub fn redact_stream_event')
redacting_sink_struct, redaction_code = extract_block(redaction_code, r'pub struct RedactingSink')
redacting_sink_impl, redaction_code = extract_block(redaction_code, r'impl RedactingSink')
redacting_sink_stream_sink_impl, redaction_code = extract_block(redaction_code, r'impl StreamSink for RedactingSink')

redaction_code = redaction_code.replace('use crate::stream::{StreamEvent, StreamSink};\n', '')

with open('src/redaction.rs', 'w') as f:
    f.write(redaction_code)

with open('src/stream/redacting.rs', 'w') as f:
    f.write(f"""use std::sync::Arc;
use crate::stream::{{StreamEvent, StreamSink}};
use crate::redaction::{{Redactor, surface}};

{redacting_sink_struct}

{redacting_sink_impl}

{redacting_sink_stream_sink_impl}

{redact_stream_event_block}
""")

with open('src/stream/mod.rs', 'r') as f:
    stream_mod_code = f.read()

stream_mod_code = stream_mod_code.replace(
    'pub mod webhook;',
    'pub mod webhook;\npub mod redacting;'
)
stream_mod_code = stream_mod_code.replace(
    'pub use broadcast::BroadcastSink;',
    'pub use broadcast::BroadcastSink;\npub use redacting::RedactingSink;'
)
with open('src/stream/mod.rs', 'w') as f:
    f.write(stream_mod_code)

def replace_in_file(filepath, old, new):
    with open(filepath, 'r') as f:
        content = f.read()
    content = content.replace(old, new)
    with open(filepath, 'w') as f:
        f.write(content)

replace_in_file('src/agent/default.rs', 'use crate::redaction::{RedactingSink, Redactor, surface};', 'use crate::redaction::{Redactor, surface};\nuse crate::stream::RedactingSink;')
replace_in_file('src/run/mini.rs', 'crate::redaction::RedactingSink', 'crate::stream::RedactingSink')

# 2. CLI -> Run
def manual_move_struct(filepath_from, filepath_to, struct_name, derives):
    with open(filepath_from, 'r') as f:
        lines = f.read().split('\n')

    start_idx = -1
    for i, line in enumerate(lines):
        if line.startswith(f"pub struct {struct_name} {{") or line.startswith(f"pub enum {struct_name} {{"):
            start_idx = i
            break

    if start_idx == -1: return

    # backtrack
    real_start = start_idx
    while real_start > 0:
        prev = lines[real_start - 1].strip()
        if prev.startswith('///') or prev.startswith('#['):
            real_start -= 1
        else:
            break

    # forward track
    brace_count = 0
    in_block = False
    end_idx = start_idx
    for i in range(start_idx, len(lines)):
        line = lines[i]
        brace_count += line.count('{')
        brace_count -= line.count('}')
        if '{' in line: in_block = True
        if in_block and brace_count == 0:
            end_idx = i + 1
            break

    block_lines = lines[real_start:end_idx]

    # Clean derives
    cleaned_block = []
    for line in block_lines:
        if line.startswith('#[derive('): continue
        cleaned_block.append(line)

    block = f"#[derive({derives})]\n" + '\n'.join(cleaned_block)

    new_from_lines = lines[:real_start] + lines[end_idx:]
    with open(filepath_from, 'w') as f:
        f.write('\n'.join(new_from_lines))

    with open(filepath_to, 'r') as f:
        code_to = f.read()

    code_to += f"\n\n{block}\n"
    with open(filepath_to, 'w') as f:
        f.write(code_to)

structs = {
    'PowerCmd': ('src/run/power.rs', 'Debug, clap::Args, Clone'),
    'ForkCmd': ('src/run/fork.rs', 'Debug, clap::Args, Clone'),
    'MergeCmd': ('src/run/merge.rs', 'Debug, clap::Args, Clone'),
    'MergeCollisionPolicy': ('src/run/merge.rs', 'Debug, Clone, Copy, clap::ValueEnum, PartialEq, Eq'),
    'MergeFormat': ('src/run/merge.rs', 'Debug, Clone, Copy, clap::ValueEnum, PartialEq, Eq'),
    'AuditCmd': ('src/run/audit.rs', 'Debug, clap::Args, Clone'),
    'BisectCmd': ('src/run/bisect.rs', 'Debug, clap::Args, Clone')
}

for name, info in structs.items():
    manual_move_struct('src/cli/args.rs', info[0], name, info[1])

# Imports
def insert_imports(filepath, imps):
    with open(filepath, 'r') as f:
        lines = f.read().split('\n')

    insert_idx = 0
    for i, line in enumerate(lines):
        if line.startswith('//!') or line.startswith('#![') or line.strip() == '':
            insert_idx = i + 1
        else:
            break

    new_lines = lines[:insert_idx] + imps + lines[insert_idx:]
    with open(filepath, 'w') as f:
        f.write('\n'.join(new_lines))

insert_imports('src/run/power.rs', ['use clap::Args;', 'use std::path::PathBuf;'])
insert_imports('src/run/fork.rs', ['use clap::Args;', 'use std::path::PathBuf;'])
insert_imports('src/run/merge.rs', ['use clap::{Args, ValueEnum};', 'use std::path::PathBuf;'])
insert_imports('src/run/audit.rs', ['use clap::Args;', 'use std::path::PathBuf;'])
insert_imports('src/run/bisect.rs', ['use clap::Args;', 'use std::path::PathBuf;'])

# Clean duplicate PathBuf
for file in set(x[0] for x in structs.values()):
    with open(file, 'r') as f:
        code = f.read()
    code = code.replace('use std::path::{Path, PathBuf};\nuse std::path::PathBuf;', 'use std::path::{Path, PathBuf};')
    code = code.replace('use std::path::PathBuf;\nuse std::path::{Path, PathBuf};', 'use std::path::{Path, PathBuf};')
    code = code.replace('use std::path::PathBuf;\n\nuse std::path::{Path, PathBuf};', 'use std::path::{Path, PathBuf};')
    with open(file, 'w') as f:
        f.write(code)

with open('src/cli/args.rs', 'r') as f:
    args_code = f.read()
imports = """use crate::run::power::PowerCmd;
use crate::run::fork::ForkCmd;
use crate::run::merge::{MergeCmd, MergeCollisionPolicy, MergeFormat};
use crate::run::audit::AuditCmd;
use crate::run::bisect::BisectCmd;
"""
args_code = args_code.replace('use clap::{Args, Subcommand, ValueEnum};\n', 'use clap::{Args, Subcommand, ValueEnum};\n' + imports)
with open('src/cli/args.rs', 'w') as f:
    f.write(args_code)

for t in set(x[0] for x in structs.values()):
    with open(t, 'r') as f:
        code = f.read()
    code = re.sub(r'use crate::cli::args::[^;]+;\n', '', code)
    with open(t, 'w') as f:
        f.write(code)

with open('src/cli/mod.rs', 'r') as f:
    code = f.read()
code = code.replace('args::PowerCmd', 'crate::run::power::PowerCmd')
code = code.replace('args::BisectCmd', 'crate::run::bisect::BisectCmd')
code = code.replace('args::AuditCmd', 'crate::run::audit::AuditCmd')
code = code.replace('args::MergeCmd', 'crate::run::merge::MergeCmd')
code = code.replace('args::ForkCmd', 'crate::run::fork::ForkCmd')
code = code.replace('args::MergeFormat', 'crate::run::merge::MergeFormat')
with open('src/cli/mod.rs', 'w') as f:
    f.write(code)
