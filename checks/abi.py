# SPDX-License-Identifier: Apache-2.0
"""The C ABI declared twice -- include/kb_engine.h for the extension,
engine/src/ffi.rs and engine/src/lib.rs for the library -- must agree member
for member: the structs the two sides fill and read, the table of entry
points, and the constants of codes and kinds.  Run by test/run.sh checks, in
a container."""
import re
import sys

root = sys.argv[1] if len(sys.argv) > 1 else "."
header = open(f"{root}/include/kb_engine.h").read()
lib = open(f"{root}/engine/src/lib.rs").read()
ffi = open(f"{root}/engine/src/ffi.rs").read()
errors = open(f"{root}/engine/src/error.rs").read()


def c_struct(name):
    m = re.search(r"typedef struct %s\s*\{(.*?)\}\s*%s;" % (name, name), header, re.S)
    if not m:
        sys.exit(f"abi: no struct {name} in the header")
    body = re.sub(r"/\*.*?\*/", "", m.group(1), flags=re.S)
    names = []
    for decl in body.split(";"):
        decl = decl.strip()
        if not decl:
            continue
        fn = re.search(r"\(\*\s*(\w+)\)", decl)
        if fn:
            names.append(fn.group(1))
        else:
            names.append(re.findall(r"(\w+)\s*(?:\[[^\]]*\])?$", decl)[0])
    return names


def rust_struct(source, name):
    m = re.search(r"pub struct %s \{(.*?)\n\}" % name, source, re.S)
    if not m:
        sys.exit(f"abi: no struct {name} in the library")
    body = re.sub(r"//[^\n]*", "", m.group(1))
    return re.findall(r"^\s*(?:pub\s+)?(\w+):", body, re.M)


failed = False
structs = [("KbError", ffi), ("KbKey", ffi), ("KbStr", ffi), ("KbTerm", ffi), ("KbDictEntry", ffi),
           ("KbColumn", ffi), ("KbSqlValue", ffi), ("KbMappingRow", ffi), ("KbHost", ffi),
           ("KbConfig", ffi), ("KbQuerySpec", ffi), ("KbUpdateSpec", ffi), ("KbLoadSpec", ffi),
           ("KbDumpSpec", ffi), ("KbHttpRequest", ffi), ("KbHttpInfo", ffi), ("KbTrace", ffi),
           ("KbApi", lib)]
for name, source in structs:
    c, r = c_struct(name), rust_struct(source, name)
    if c != r:
        failed = True
        print(f"abi: {name} differs")
        for i in range(max(len(c), len(r))):
            a = c[i] if i < len(c) else "-"
            b = r[i] if i < len(r) else "-"
            print(f"   {'  ' if a == b else '!='} {a:28} {b}")
    else:
        print(f"abi: {name}: {len(c)} members agree")

# the constants: every #define KB_X n of the header, as pub const KB_X: i32 = n
rust_consts = dict(re.findall(r"pub const (KB_\w+): \w+ = (\d+);", ffi + errors))
for name, value in re.findall(r"#define (KB_\w+)\s+(\d+)\b", header):
    if name in ("KB_ABI_VERSION",):
        continue
    if name not in rust_consts:
        print(f"abi: {name} is not in the library")
        failed = True
    elif rust_consts[name] != value:
        print(f"abi: {name} is {value} in the header, {rust_consts[name]} in the library")
        failed = True
abi = re.search(r"#define KB_ABI_VERSION\s+(\d+)", header).group(1)
if not re.search(r"const ABI_VERSION: u32 = %s;" % abi, lib):
    print(f"abi: KB_ABI_VERSION {abi} is not the library's")
    failed = True
print("abi: the constants agree" if not failed else "abi: FAILED")
sys.exit(1 if failed else 0)
