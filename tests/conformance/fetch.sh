#!/usr/bin/env bash
# 一致性测试向量获取/生成脚本（向量二进制不入库；产物在本地缓存目录，git 忽略）。
#
# 2026-10-24 施工图①裁决：外网被墙环境下就位集为「生成配方」向量——
# 本脚本按 manifest.toml 的 generate 命令重放（{OUT} 替换为缓存路径）
# 并核对 sha256；source 向量（下载型，网络恢复后启用）走 curl。
#
# 生成后 sha 不匹配（如换了 ffmpeg 版本重生成）：**保留文件**并告警——
# runner 会照跑且在摘要行落 sha_ok=false（漂移是信号，不是 skip）。
# 下载后 sha 不匹配视为传输损坏：删除重试。
#
# 用法：
#   bash tests/conformance/fetch.sh [清单路径] [缓存目录]
# 环境变量 YE_CONFORMANCE_CACHE 覆盖默认缓存目录。
# 依赖：ffmpeg（生成型向量）与 curl+sha256sum（下载型向量）。
set -euo pipefail
cd "$(dirname "$0")"

MANIFEST="${1:-manifest.toml}"
CACHE="${2:-${YE_CONFORMANCE_CACHE:-.cache}}"
[[ -f "$MANIFEST" ]] || { echo "fetch: 清单不存在：$MANIFEST" >&2; exit 2; }
mkdir -p "$CACHE"

ok=0 cached=0 fail=0 pending=0

# 行式 TOML 子集提取向量五元组 (name, container, source, generate, sha256)。
# 空字段以 "-" 哨兵传递（bash 对 IFS 制表符会折叠空字段，导致错位）。
while IFS=$'\t' read -r name container source generate sha; do
  [[ "$source" == "-" ]] && source=""
  [[ "$generate" == "-" ]] && generate=""
  file="$CACHE/$name.$container"
  sha_ok_file() { [[ "$(sha256sum "$file" | cut -d' ' -f1)" == "$sha" ]]; }
  if [[ -f "$file" ]] && sha_ok_file; then
    echo "已缓存 $name（sha256 一致）"
    cached=$((cached + 1))
    continue
  fi
  if [[ -n "$generate" ]]; then
    echo "生成 $name（配方重放）"
    cmd="${generate//\{OUT\}/$file}"
    if bash -c "$cmd"; then
      if sha_ok_file; then
        ok=$((ok + 1))
      else
        echo "sha256 不符 $name：期望 $sha 实得 $(sha256sum "$file" | cut -d' ' -f1)——文件保留，runner 将落 sha_ok=false（多半是 ffmpeg 版本变化，核对 manifest 的 ffmpeg_version）" >&2
        fail=$((fail + 1))
      fi
    else
      echo "生成失败 $name" >&2
      fail=$((fail + 1))
    fi
  elif [[ -n "$source" ]]; then
    echo "拉取 $name ← $source"
    if curl -fL --retry 3 --connect-timeout 20 -o "$file" "$source"; then
      if sha_ok_file; then
        ok=$((ok + 1))
      else
        echo "sha256 不符 $name（下载损坏）：已删除，可重跑本脚本重试" >&2
        rm -f "$file"
        fail=$((fail + 1))
      fi
    else
      echo "下载失败 $name（$source）" >&2
      rm -f "$file"
      fail=$((fail + 1))
    fi
  else
    echo "向量 $name 既无 generate 也无 source（清单错误）" >&2
    fail=$((fail + 1))
  fi
done < <(
  awk '
    function val(line) { sub(/^[^=]*= */, "", line); gsub(/"/, "", line); return line }
    function or_dash(v) { return v == "" ? "-" : v }
    function flush() {
      if (n != "") print n "\t" or_dash(c) "\t" or_dash(s) "\t" or_dash(g) "\t" or_dash(h)
      n = c = s = g = h = ""
    }
    /^ *\[\[vector\]\]/           { flush(); inv = 1; next }
    /^ *\[\[pending_vector\]\]$/  { flush(); inv = 0; next }
    inv && /^ *name *=/       { n = val($0) }
    inv && /^ *container *=/  { c = val($0) }
    inv && /^ *source *=/     { s = val($0) }
    inv && /^ *generate *=/   { g = val($0) }
    inv && /^ *sha256 *=/     { h = val($0) }
    END { flush() }
  ' "$MANIFEST"
)

# 待网络清册：只登记不取（网络恢复后实测 sha256 升入 [[vector]]）
pending=$(grep -c '^\[\[pending_vector\]\]' "$MANIFEST" || true)

echo "fetch 完成：新就位 $ok，已缓存 $cached，失败 $fail，待网络 $pending"
[[ "$fail" -eq 0 ]]
