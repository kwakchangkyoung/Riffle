#!/bin/bash
# Phase 10 필터 벤치마크: riffle CLI filter vs grep/awk 대조
cd "$(dirname "$0")/.."
CLI=./target/release/examples/cli
F=testdata/big_10g.log

echo "=== 1. OR include: --inc ERROR --inc WARN ==="
/usr/bin/time -l $CLI "$F" filter --inc ERROR --inc WARN 2>&1 | grep -E "보이는|시간|RSS|maximum resident"
echo "--- grep ---"
/usr/bin/time -l bash -c "LC_ALL=C grep -cE 'ERROR|WARN' '$F'" 2>&1 | grep -vE "messages|signals|context switches|instructions|cycles|memory footprint"

echo "=== 2. AND include: --and --inc 결제 --inc APPROVED ==="
/usr/bin/time -l $CLI "$F" filter --and --inc 결제 --inc APPROVED 2>&1 | grep -E "보이는|시간|RSS|maximum resident"
echo "--- grep ---"
/usr/bin/time -l bash -c "LC_ALL=C grep 결제 '$F' | grep -c APPROVED" 2>&1 | grep -vE "messages|signals|context switches|instructions|cycles|memory footprint"

echo "=== 3. exclude: --exc heartbeat ==="
/usr/bin/time -l $CLI "$F" filter --exc heartbeat 2>&1 | grep -E "보이는|시간|RSS|maximum resident"
echo "--- grep ---"
/usr/bin/time -l bash -c "LC_ALL=C grep -vc heartbeat '$F'" 2>&1 | grep -vE "messages|signals|context switches|instructions|cycles|memory footprint"

echo "=== 4. level: --level error ==="
/usr/bin/time -l $CLI "$F" filter --level error 2>&1 | grep -E "보이는|시간|RSS|maximum resident"
echo "--- awk ---"
/usr/bin/time -l awk '
function isword(c) { return c ~ /[A-Za-z0-9_]/ }
{
    line = substr($0, 1, 200)
    pos = 1
    while (pos <= length(line)) {
        tail = substr(line, pos)
        if (!match(tail, /(ERROR|FATAL|WARN(ING)?|INFO|DEBUG|TRACE)/)) break
        s = pos + RSTART - 1
        e = s + RLENGTH - 1
        before = (s == 1) ? "" : substr(line, s - 1, 1)
        after = (e >= length(line)) ? "" : substr(line, e + 1, 1)
        if (!isword(before) && !isword(after)) {
            tok = substr(line, s, RLENGTH)
            if (tok == "ERROR" || tok == "FATAL") c++
            break
        }
        pos = e + 1
    }
}
END { print c + 0 }
' "$F" 2>&1 | grep -vE "messages|signals|context switches|instructions|cycles|memory footprint"

echo "=== 5. intersection: --inc ERROR --search 결제 ==="
/usr/bin/time -l $CLI "$F" filter --inc ERROR --search 결제 2>&1 | grep -E "보이는|교집합|시간|RSS|maximum resident"
echo "--- grep ---"
/usr/bin/time -l bash -c "LC_ALL=C grep ERROR '$F' | grep -c 결제" 2>&1 | grep -vE "messages|signals|context switches|instructions|cycles|memory footprint"

echo "=== DONE ==="
