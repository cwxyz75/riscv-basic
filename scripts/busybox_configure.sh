#!/bin/bash
# Regenerate third_party/busybox/{config,files.txt,gen/} (UV8): configure the fetched BusyBox 1.36.1 sources in a scratch copy
# with the applets named below, let Kbuild generate its headers (it fails later at the compile step, which is not wanted), and
# keep the configuration, the list of source files the configuration builds and the generated headers. Needs `make fetch-third-party`
# first, a host C compiler and python3. The compile itself is done by init/build.rs.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
src="$root/third_party/fetched/busybox/src"
[ -f "$src/Makefile" ] || { echo "busybox: run 'make fetch-third-party' first" >&2; exit 1; }
work="$(mktemp -d)"
trap '[ -n "${KEEP:-}" ] || rm -rf "$work"' EXIT
cp -R "$src" "$work/src"
cd "$work/src"
# macOS host tools need <byteswap.h>
mkdir -p "$work/shim"
printf '#define bswap_16(x) __builtin_bswap16(x)\n#define bswap_32(x) __builtin_bswap32(x)\n#define bswap_64(x) __builtin_bswap64(x)\n' > "$work/shim/byteswap.h"
make allnoconfig HOSTCC=cc >/dev/null 2>&1 || true
python3 - "$@" <<'PY'
import re
APPLETS = ['BASENAME','DIRNAME','WC','HEAD','TAIL','UNIQ','CUT','TR','SEQ','TEE','SORT','MD5SUM','SHA256SUM','CAT','ECHO','TRUE','FALSE','NL','FOLD','EXPAND','UNEXPAND','REV','TAC','COMM','PASTE','CMP','OD','HEXDUMP','XXD','CKSUM','SHA1SUM','SHA512SUM','BASE64','YES','SLEEP','PRINTF','TEST','TEST1','TEST2','MKDIR','RM','LN','TOUCH','DD','TRUNCATE','FACTOR','SPLIT','FALSE','TRUE','GREP','EGREP','FGREP','SED','AWK','EXPR','FIND','LS','CP','MV','DIFF','DATE','UNAME','ID','WHOAMI','STAT','READLINK','REALPATH','CHMOD','MKTEMP','DU','DF','WHICH','LOGNAME','GROUPS','PWD','DIRNAME','TIMEOUT','KILL','HUSH','XARGS','ENV','NOHUP','TIME','TSORT','SUM','SHUF','NPROC','INSTALL','RMDIR','LINK','UNLINK','MKFIFO','TOUCH','TR','OD']
t = open('.config').read()
def setopt(name, val='y'):
    global t
    pat = re.compile(r'^(# CONFIG_%s is not set|CONFIG_%s=.*)$' % (name, name), re.M)
    line = 'CONFIG_%s=%s' % (name, val)
    t = pat.sub(line, t) if pat.search(t) else t + line + '\n'
FEATURES = ['FEATURE_SH_STANDALONE','FEATURE_SH_MATH','FEATURE_SH_MATH_64','FEATURE_SH_MATH_BASE','HUSH_TICK','HUSH_IF','HUSH_LOOPS','HUSH_CASE','HUSH_FUNCTIONS','HUSH_LOCAL','HUSH_ECHO','HUSH_PRINTF','HUSH_TEST','HUSH_EXPORT','HUSH_READONLY','HUSH_KILL','HUSH_WAIT','HUSH_TRAP','HUSH_TYPE','HUSH_UNSET','HUSH_SET','HUSH_COMMAND','HUSH_UMASK','HUSH_MODE_X','HUSH_LINENO_VAR','HUSH_EXPORT_N','HUSH_SHIFT','HUSH_CD','HUSH_PWD','HUSH_EVAL','HUSH_EXEC','HUSH_SOURCE','HUSH_GETOPTS','HUSH_UNSET','HUSH_READ','HUSH_BREAK','HUSH_CONTINUE','HUSH_TRUE','HUSH_FALSE','HUSH_RETURN','HUSH_EXIT','FEATURE_PREFER_APPLETS','FEATURE_FIND_EXEC','FEATURE_FIND_EXEC_PLUS','FEATURE_XARGS_SUPPORT_QUOTES','FEATURE_XARGS_SUPPORT_TERMOPT','FEATURE_XARGS_SUPPORT_ZERO_TERM','FEATURE_ENV_LONG_OPTIONS','FEATURE_LS_FILETYPES','FEATURE_LS_FOLLOWLINKS','FEATURE_LS_RECURSIVE','FEATURE_LS_SORTFILES','FEATURE_LS_TIMESTAMPS','FEATURE_LS_USERNAME','FEATURE_LS_WIDTH','FEATURE_SORT_BIG','FEATURE_FIND_TYPE','FEATURE_FIND_MAXDEPTH','FEATURE_FIND_PRUNE','FEATURE_FIND_PATH','FEATURE_FIND_REGEX','FEATURE_FIND_SIZE','FEATURE_FIND_DELETE','FEATURE_FIND_MTIME','FEATURE_FIND_NEWER','FEATURE_FIND_PERM','FEATURE_FIND_NOT','FEATURE_FIND_PAREN','FEATURE_FIND_EMPTY','FEATURE_FANCY_HEAD','FEATURE_FANCY_TAIL','FEATURE_GREP_CONTEXT','FEATURE_GREP_EGREP_ALIAS','FEATURE_GREP_FGREP_ALIAS','FEATURE_WC_LARGE','FEATURE_AWK_GNU_FUNCS','FEATURE_AWK_LIBM','FEATURE_SEQ_PRECISION','FEATURE_SED_CHECK_FOLLOWS','FEATURE_CP_REFLINK','FEATURE_PRESERVE_HARDLINKS','FEATURE_CUT_REGEX','FEATURE_MD5_SHA1_SUM_CHECK','FEATURE_TR_CLASSES','FEATURE_TR_EQUIV','FEATURE_EXPAND_LONG_OPTIONS','FEATURE_UNIQ_REPEAT','FEATURE_CAT_NUMBER' ]
for o in ['NOMMU', 'LFS', 'BUSYBOX'] + APPLETS + FEATURES:
    setopt(o)
# programs started by programs go through the shell as `/bin/busybox` (a spawn grant, OD118), not `/proc/self/exe`
setopt('BUSYBOX_EXEC_PATH', '"/bin/busybox"')
# the shell choices: hush (`busybox hush`; ash cannot run without an MMU), not as `sh`: the system shell is `ish`
t = re.sub(r'^(# )?CONFIG_(SH_IS_\w+|BASH_IS_\w+)\b.*\n', '', t, flags=re.M)
t += 'CONFIG_SH_IS_NONE=y\nCONFIG_BASH_IS_NONE=y\n'
open('.config', 'w').write(t)
PY
yes '' | make oldconfig HOSTCC=cc >/dev/null 2>&1 || true
# Kbuild generates include/*.h before it gets to compiling; a CC that compiles nothing lets it get that far
cat > "$work/fakecc.sh" <<'FAKE'
#!/bin/bash
out=""; prev=""; dep=""
for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; case "$a" in -Wp,-MD,*) dep="${a#-Wp,-MD,}";; esac; prev="$a"; done
[ -n "$out" ] && : > "$out"; [ -n "$dep" ] && echo "$out: " > "$dep"; exit 0
FAKE
chmod +x "$work/fakecc.sh"
make -k HOSTCC=cc HOSTCFLAGS="-I$work/shim" CC="$work/fakecc.sh" LD=/bin/true AR=/bin/true SKIP_STRIP=y busybox_unstripped >"$work/make.log" 2>&1 || true
out="$root/third_party/busybox"
mkdir -p "$out/gen"
cp .config "$out/config"
for f in NUM_APPLETS.h applet_tables.h applets.h autoconf.h bbconfigopts.h bbconfigopts_bz2.h common_bufsiz.h embedded_scripts.h usage.h usage_compressed.h; do
  [ -f "include/$f" ] || { echo "busybox: Kbuild did not generate include/$f" >&2; exit 1; }
  cp "include/$f" "$out/gen/$f"
done
python3 - "$out/files.txt" <<'PY'
import os, re, sys
cfg = {}
for l in open('.config'):
    m = re.match(r'CONFIG_(\w+)=(.*)', l.strip())
    if m: cfg[m.group(1)] = m.group(2)
files = set()
for d, _, names in os.walk('.'):
    if 'Kbuild' not in names: continue
    d = os.path.normpath(d)
    if d.startswith(('scripts', 'docs', 'testsuite', 'examples')): continue
    txt = open(os.path.join(d, 'Kbuild')).read().replace('\\\n', ' ')
    for l in txt.split('\n'):
        m = re.match(r'\s*lib-(\S+)\s*[:+]?=\s*(.*)', l)
        if not m: continue
        cond, objs = m.groups()
        c = re.match(r'\$\(CONFIG_(\w+)\)', cond)
        if cond == 'y' or (c and cfg.get(c.group(1)) == 'y'):
            for o in objs.split():
                if o.endswith('.o') and os.path.exists(os.path.join(d, o[:-2] + '.c')):
                    files.add(os.path.join(d, o[:-2] + '.c'))
open(sys.argv[1], 'w').write('\n'.join(sorted(files)) + '\n')
PY
echo "busybox: $(wc -l < "$out/files.txt") source files, $(grep -c '=y' "$out/config") options set"
