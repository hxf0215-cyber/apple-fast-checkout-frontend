# -*- coding: utf-8 -*-
"""
Apple 商城 —— 纯算法 HTTP 服务（★ 全程不联网，无任何外部请求）

启动：
    python apple_algo.py                 # 默认 127.0.0.1:8788
    python apple_algo.py --port 9000 --host 0.0.0.0

调用（GET 用 query，POST 用 JSON body，两者等价）：
    curl "http://127.0.0.1:8788/hashcash?bits=3&challenge=abc"
    curl "http://127.0.0.1:8788/decode_pltn?ssi=4AAABoOY..."
    curl -X POST "http://127.0.0.1:8788/srp_evidence" -d "{...}"
    curl "http://127.0.0.1:8788/graviton_headers?stk=xxx"
    curl "http://127.0.0.1:8788/fetch_call_id"

统一响应：
    {"ok": true,  "data": <值>}
    {"ok": false, "error": "<原因>"}

★ 本服务只做本地计算：SRP-6a / hashcash / pltn 解码 / shld PoW 求解 /
  graviton 头拼装 / fetch-call-id。不发送任何网络请求，不需要 cookie。
"""
import sys, os, json, time, re, base64, hashlib, secrets, argparse, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs

sys.stdout.reconfigure(encoding="utf-8", errors="replace")

# ══════════════════════════════════════════════════════════════
#  纯算法（全部本地计算，不联网）
# ══════════════════════════════════════════════════════════════

# ── SRP-6a 群常量：RFC 5054 Appendix A 2048-bit ──
N = int("AC6BDB41324A9A9BF166DE5E1389582FAF72B6651987EE07FC3192943DB56050A37329CBB4"
        "A099ED8193E0757767A13DD52312AB4B03310DCD7F48A9DA04FD50E8083969EDB767B0CF60"
        "95179A163AB3661A05FBD5FAAAE82918A9962F0B93B855F97993EC975EEAA80D740ADBF4FF"
        "747359D041D5C33EA71D281E446B14773BCA97B43A23FB801676BD207A436C6481F1D2B907"
        "8717461A5B9D32E688F87748544523B524B0D57D5EA77A2775D2ECFA032CFBDBF52FB37861"
        "60279004E57AE6AF874E7303CE53299CCC041C7BC308D82A5698F3A8D0C38271AE35F8E9DB"
        "FBB694B5C803D89F7AE435DE236D525F54759B65E372FCD68EF20FA7111F9E4AFF73", 16)
G = 2
NB = 256


def _pad(x): return x.to_bytes(NB, "big")

def _H(*p):
    h = hashlib.sha256()
    for x in p:
        h.update(x if isinstance(x, bytes) else x.encode())
    return h.digest()

def _xor(a, b): return bytes(x ^ y for x, y in zip(a, b))

def _b64(b): return base64.b64encode(b).decode()

def _b64d(x):
    """容错 base64 解码：去空白 + 自动补 padding"""
    x = "".join(str(x).split())
    return base64.b64decode(x + "=" * (-len(x) % 4))


def srp_evidence(account, password, salt_b64, B_b64, iterations,
                 protocol="s2k", a=None):
    """SRP-6a → (A, M1, M2, K)，全部 base64

    ★ 纯算法：密码永不上行。init 与 complete 必须用同一个 a。
    """
    s = _b64d(salt_b64)
    B = int.from_bytes(_b64d(B_b64), "big")
    I = account.lower().encode()                        # ★ 强制小写
    a = a if a is not None else secrets.randbits(256)
    Apad = _pad(pow(G, a, N))                           # ★ 左补零到 256 字节

    pw_h = _H(password)
    key = pw_h if protocol == "s2k" else pw_h.hex().encode()
    P = hashlib.pbkdf2_hmac("sha256", key, s, int(iterations), dklen=32)

    x = int.from_bytes(_H(s, _H(b"", b":", P)), "big")             # ②
    k = int.from_bytes(_H(_pad(N), _pad(G)), "big")                # ③
    u = int.from_bytes(_H(Apad, _pad(B)), "big")                   # ④
    S = pow((B - k * pow(G, x, N)) % N, a + u * x, N)              # ⑤
    K = _H(_pad(S))                                                # ⑥
    M1 = _H(_xor(_H(_pad(N)), _H(_pad(G))), _H(I), s, Apad, _pad(B), K)  # ⑦ XOR
    M2 = _H(Apad, M1, K)                                           # ⑧
    return _b64(Apad), _b64(M1), _b64(M2), _b64(K)


def srp_public(a=None):
    """SRP-6a 第 1 趟：只算客户端公钥 A（★ 不需要 salt/b）

    用于在 1-2 signin/init 之前生成要上传的 A。
    a 不传则随机生成并返回，调用方需保存该 a 供第 2 趟算 m1/m2 使用。
    → (A_b64, a)
    """
    a = a if a is not None else secrets.randbits(256)
    return _b64(_pad(pow(G, int(a), N))), int(a)


def hashcash(bits, challenge, max_iter=5_000_000):
    """hashcash PoW → X-APPLE-HC 头的值

    ★ 实测：摘要 SHA-1，分隔符 "::"，校验二进制摘要连续前导零位数 >= bits
    """
    ts = time.strftime("%Y%m%d%H%M%S", time.localtime())
    need = int(bits)
    for c in range(max_iter):
        stamp = "%s::%d" % (challenge, c)
        d = hashlib.sha1(stamp.encode()).digest()
        zeros = 0
        for byte in d:
            if byte == 0:
                zeros += 8
                continue
            for i in range(7, -1, -1):
                if (byte >> i) & 1: break
                zeros += 1
            break
        if zeros >= need:
            return "1:%s:%s:%s" % (bits, ts, stamp)
    return None


def decode_pltn(ssi):
    """从 ssi 解出 pltn 系列码

    ssi 结构 = base64url( 'K' + 目标URL(?pltn=XXXX||;PART|) + \\x00 + 二进制签名块 )
    ★ ssi 里没有 "BGAAAA" 分隔符，直接整体解码即可
    """
    if not ssi: return None
    for cand in (ssi, ssi.split("AAAA")[-1], ssi.split("BGAAAA")[-1]):
        try:
            d = base64.urlsafe_b64decode(cand + "=" * (-len(cand) % 4))
        except Exception:
            continue
        m = re.search(rb"pltn=([0-9A-Za-z|;.\-]+)", d)
        if m:
            return m.group(1).decode("utf-8", "replace").split("||;")[0]
    return None


def solve_shld(result, parts, low, high):
    """shld v2_1 整数分解 PoW → list[int]

    ★ 找 parts 个 [low,high] 整数使乘积 == result
      剪枝：只遍历 result 的因子、升序非降序、部分积超限即 break、
      非末位时部分积必须整除 result
    ★ 返回值必须是数字数组（字符串数组会导致 POST 返回 404）
    """
    R = int(result)
    divs = [d for d in range(int(low), int(high) + 1) if R % d == 0]
    out = []
    def rec(prod, idx, depth):
        if depth == parts: return prod == R
        for i in range(idx, len(divs)):
            d = divs[i]
            p2 = prod * d
            if p2 > R: break
            if R % p2 != 0 and depth + 1 < parts: continue
            out.append(d)
            if rec(p2, i, depth + 1): return True      # i 不+1 → 允许重复(非降序)
            out.pop()
        return False
    return list(out) if rec(1, 0, 0) else None


def solve_shld_v2(salt, challenge, algorithm="SHA-256", max_iter=1_000_000):
    """shld v2_1 的 challenge_version == 2 → SHA 暴力 PoW → int

    ★ 求最小 i 使 digest(algorithm, salt + str(i)) == challenge
      （还原自 verify.js _0x58d9de / _0x445cf5；本次部署未出现 v2，按源码保留）
    """
    alg = (algorithm or "SHA-256").upper().replace("-", "")
    if alg not in ("SHA256", "SHA1", "SHA512", "MD5"):
        alg = "SHA256"
    name = {"SHA256": "sha256", "SHA1": "sha1",
            "SHA512": "sha512", "MD5": "md5"}[alg]
    for i in range(int(max_iter)):
        if hashlib.new(name, (str(salt) + str(i)).encode()).hexdigest() == challenge:
            return i
    return None


def solve_shld_auto(ch):
    """★ 传入完整挑战 dict，自动按 challenge_version 分发求解

    ch = GET /shop/shld/work/v2_1/q 返回的原始 dict
    → 返回 number（v1 为数字数组；v2 为整数）
    """
    if not isinstance(ch, dict):
        return {"error": "ch 必须是挑战 dict"}
    ver = int(ch.get("challenge_version", 1))
    if ver == 1:
        for k in ("result", "parts", "low", "high"):
            if ch.get(k) is None: return {"error": "v1 缺少 " + k}
        return solve_shld(ch["result"], int(ch["parts"]),
                          int(ch["low"]), int(ch["high"]))
    elif ver == 2:
        if ch.get("challenge") is None: return {"error": "v2 缺少 challenge"}
        return solve_shld_v2(ch.get("salt", ""), ch["challenge"],
                             ch.get("algorithm", "SHA-256"))
    return {"error": "未知 challenge_version " + str(ver)}


def shld_body(ch, number=None, took=0):
    """构造 shld POST 请求体文本（本地拼装，不发送）

    ch      GET 挑战返回的原始 dict（原样回传）
    number  解（数字数组）；不传则自动求解
    took    求解耗时 ms
    ★ Content-Type 必须是 text/plain;charset=UTF-8
    """
    if number is None:
        t0 = time.time()
        number = solve_shld(ch.get("result"), int(ch.get("parts", 6)),
                            int(ch.get("low", 100)), int(ch.get("high", 400)))
        took = int((time.time() - t0) * 1000)
    body = {}
    for k, v in ch.items():
        body[k] = v                                     # 原题字段原样
    body["number"] = number                             # ★ 数字数组
    body["took"] = took
    body["flagskv"] = {"patSkip": True}
    body["jsa"] = {"s": 255, "f": []}
    return json.dumps(body, ensure_ascii=False)


def fetch_call_id():
    """graviton 头 x-aos-ui-fetch-call-1（毫秒时间戳 hex）"""
    return format(int(time.time() * 1000), "x")


def graviton_headers(stk=None, page="checkoutPage", ref=None, origin=None):
    """graviton 通用请求头（本地拼装）"""
    h = {"x-aos-model-page": page, "modelVersion": "v2", "syntax": "graviton",
         "X-Requested-With": "Fetch",
         "Content-Type": "application/x-www-form-urlencoded",
         "x-aos-ui-fetch-call-1": fetch_call_id(), "Accept": "*/*",
         "sec-ch-ua": '"Chromium";v="154", "Microsoft Edge";v="154", "Not A(Brand";v="99"',
         "sec-ch-ua-mobile": "?0", "sec-ch-ua-platform": '"Windows"'}
    if stk: h["x-aos-stk"] = stk
    if ref: h["Referer"] = ref
    if origin: h["Origin"] = origin
    return h


# ══════════════════════════════════════════════════════════════
#  路由
# ══════════════════════════════════════════════════════════════
def _srp(p):
    need = ("account", "password", "salt", "b", "iterations")
    miss = [k for k in need if not p.get(k)]
    if miss: return {"error": "缺少参数: " + ",".join(miss)}
    a = p.get("a")
    A_, M1, M2, K = srp_evidence(p["account"], p["password"], p["salt"], p["b"],
                                 int(p["iterations"]), p.get("protocol", "s2k"),
                                 int(a) if a else None)
    return {"A": A_, "M1": M1, "M2": M2, "K": K, "a": str(a) if a else None}

def _srp_public(p):
    a = p.get("a")
    A_b64, a_val = srp_public(int(a) if a else None)
    return {"A": A_b64, "a": str(a_val)}

def _hashcash(p):
    if not p.get("challenge"): return {"error": "需要 challenge"}
    return hashcash(int(p.get("bits", 3)), p["challenge"])

def _pltn(p):
    if not p.get("ssi"): return {"error": "需要 ssi"}
    return decode_pltn(p["ssi"])

def _solve_shld(p):
    """v1 整数分解；若传了 ch(dict) 则自动按版本分发"""
    ch = p.get("ch")
    if isinstance(ch, dict):
        return solve_shld_auto(ch)
    miss = [k for k in ("result", "parts", "low", "high") if p.get(k) is None]
    if miss: return {"error": "缺少参数: " + ",".join(miss)}
    return solve_shld(p["result"], int(p["parts"]), int(p["low"]), int(p["high"]))

def _solve_shld_v2(p):
    if p.get("challenge") is None: return {"error": "需要 challenge"}
    return solve_shld_v2(p.get("salt", ""), p["challenge"],
                         p.get("algorithm", "SHA-256"),
                         int(p.get("max_iter", 1000000)))

def _shld_body(p):
    ch = p.get("ch")
    if not isinstance(ch, dict): return {"error": "需要 ch（挑战原始 dict），建议 POST"}
    return shld_body(ch, p.get("number"), int(p.get("took", 0)))

def _graviton(p):
    return graviton_headers(p.get("stk"), p.get("page", "checkoutPage"),
                            p.get("ref"), p.get("origin"))

ROUTES = {
    "/srp_evidence":     (_srp,      "SRP-6a 证据（参:account,password,salt,b,iterations,a?,protocol?）→ {A,M1,M2,K}"),
    "/srp_public":       (_srp_public, "SRP 第1趟：只算客户端公钥 A（参:a?，不传则随机）→ {A,a}"),
    "/hashcash":         (_hashcash, "X-APPLE-HC 头值（参:bits=3,challenge=xxx）"),
    "/decode_pltn":      (_pltn,     "从 ssi 解 pltn 系列码（参:ssi=xxx）"),
    "/shld/solve":       (_solve_shld, "shld PoW 求解★自动（POST 传 ch=挑战dict 自动判 version；或 GET 传 result,parts,low,high 走 v1）→ number"),
    "/shld/solve_v1":    (_solve_shld, "shld v1 整数分解（参:result,parts,low,high）→ 数字数组"),
    "/shld/solve_v2":    (_solve_shld_v2, "shld v2 SHA 暴力（参:challenge,salt?,algorithm?）→ 整数 i"),
    "/shld/body":        (_shld_body, "shld POST 请求体文本（POST 传 ch=挑战dict，可选 number,took）"),
    "/fetch_call_id":    (lambda p: fetch_call_id(), "graviton 头 x-aos-ui-fetch-call-1"),
    "/graviton_headers": (_graviton, "graviton 通用请求头（参:stk,page,ref,origin）"),
}


class Handler(BaseHTTPRequestHandler):
    server_version = "AppleAlgoService/1.0"

    def _send(self, code, obj):
        body = json.dumps(obj, ensure_ascii=False).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _params(self):
        u = urlparse(self.path)
        p = {k: v[0] for k, v in parse_qs(u.query).items()}
        ln = int(self.headers.get("Content-Length") or 0)
        if ln:
            try:
                j = json.loads(self.rfile.read(ln).decode("utf-8"))
                if isinstance(j, dict):
                    p.update({k: v for k, v in j.items() if v is not None})
            except Exception: pass
        return u.path.rstrip("/") or "/", p

    def _handle(self):
        path, p = self._params()
        if path in ("/", "/help", "/list"):
            self._send(200, {"ok": True, "note": "纯算法服务，不联网",
                             "data": {k: {"desc": v[1]} for k, v in ROUTES.items()}})
            return
        if path == "/ping":
            self._send(200, {"ok": True, "data": "pong"}); return
        if path not in ROUTES:
            self._send(404, {"ok": False, "error": "未知接口 " + path}); return
        try:
            out = ROUTES[path][0](p)
            if isinstance(out, dict) and "error" in out:
                self._send(400, {"ok": False, "error": out["error"]})
            else:
                self._send(200, {"ok": True, "data": out})
        except Exception as e:
            import traceback
            self._send(500, {"ok": False, "error": str(e),
                             "trace": traceback.format_exc()[-300:]})

    def do_GET(self): self._handle()
    def do_POST(self): self._handle()
    def log_message(self, fmt, *args):
        sys.stderr.write("%s %s\n" % (time.strftime("%H:%M:%S"), fmt % args))


# ══════════════════════════════════════════════════════════════
#  启动端口选择（★ 新增）
#    ① 命令行 --port 指定      → 直接用，不问
#    ② 启动时交互询问         → 直接回车 = 8788
#    ③ 非交互环境（无 stdin）  → 直接用 8788，不问
# ══════════════════════════════════════════════════════════════
DEFAULT_PORT = 8788


def ask_port(default=DEFAULT_PORT):
    """交互式询问端口；直接回车返回默认值。非交互/异常时静默返回默认值。"""
    try:
        if not sys.stdin or not sys.stdin.isatty():
            return default
    except Exception:
        return default
    try:
        s = input("请输入端口（直接回车用 %d）：" % default).strip()
    except (EOFError, KeyboardInterrupt):
        print()
        return default
    except Exception:
        return default
    if not s:
        return default
    try:
        v = int(s)
    except ValueError:
        print("端口必须是数字，已改用默认 %d" % default)
        return default
    if not (1 <= v <= 65535):
        print("端口需在 1~65535，已改用默认 %d" % default)
        return default
    return v


def main():
    ap = argparse.ArgumentParser(description="Apple 纯算法 HTTP 服务（不联网）")
    ap.add_argument("--host", default="0.0.0.0")
    ap.add_argument("--port", type=int, default=None,
                    help="不传则启动时询问，直接回车用 %d" % DEFAULT_PORT)
    a = ap.parse_args()

    port = a.port if a.port is not None else ask_port()

    try:
        srv = ThreadingHTTPServer((a.host, port), Handler)
    except OSError as e:
        print("端口 %d 启动失败：%s" % (port, e))
        print("可换一个端口重试，例如：python apple_algo.py --port 8789")
        return

    print("纯算法服务已启动 http://%s:%d  （不联网）" % (a.host, port))
    print("接口列表 http://%s:%d/" % (a.host, port))
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        print("\n已停止")


if __name__ == "__main__":
    main()
