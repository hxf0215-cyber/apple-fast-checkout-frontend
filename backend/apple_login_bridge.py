import sys, os, json, time, requests, warnings
warnings.filterwarnings("ignore")
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import apple_algo as algo

IDMSA = "https://idmsa.apple.com.cn"
WIDGET_KEY = "a797929d224abb1cc663bb187bbcd02f7172ca3a84df470380522a7c6092118b"
STATE_DIR = "_login_states"
UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36"
FDC = '{"U":"Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36","L":"zh-CN","Z":"GMT+08:00","V":"1.1","F":"Fla44j1e3NlY5BNlY5BSmHACVZXnNAAqW1c6Med9FrurJhBR.uMp4UdHz13NldjV2pNk0ug9WJ.vgRc6eWUWujsK25BNlY5cklY5BqNAE.lTjV.Dj4"}'

def new_session():
    s = requests.Session()
    s.verify = False
    state = "auth-" + __import__("secrets").token_hex(8)
    s.headers.update({
        "User-Agent": UA,
        "Accept-Language": "zh-CN,zh;q=0.9",
        "Referer": "https://idmsa.apple.com.cn/",
        "Origin": "https://idmsa.apple.com.cn",
        "X-Apple-Widget-Key": WIDGET_KEY,
        "X-Apple-OAuth-Client-Id": WIDGET_KEY,
        "X-Apple-App-Id": WIDGET_KEY,
        "X-Apple-OAuth-Client-Type": "firstPartyAuth",
        "X-Apple-OAuth-Response-Mode": "web_message",
        "X-Apple-OAuth-Response-Type": "code",
        "X-Apple-OAuth-Redirect-URI": "https://secure11.www.apple.com.cn",
        "X-Apple-OAuth-State": state,
        "X-Apple-Frame-Id": state,
        "X-Apple-Locale": "zh_CN",
        "X-Apple-Domain-Id": "48",
        "X-Apple-I-FD-Client-Info": FDC,
        "X-Requested-With": "XMLHttpRequest",
        "Content-Type": "application/json",
        "Accept": "application/json, text/javascript, */*; q=0.01",
    })
    return s

def save_state(login_id, s, account):
    os.makedirs(STATE_DIR, exist_ok=True)
    state = {
        "account": account,
        "cookies": s.cookies.get_dict(),
        "headers": {
            "scnt": s.headers.get("scnt", ""),
            "X-Apple-ID-Session-Id": s.headers.get("X-Apple-ID-Session-Id", ""),
        },
    }
    path = os.path.join(STATE_DIR, f"{login_id}.json")
    with open(path, "w", encoding="utf-8") as f:
        json.dump(state, f)

def load_state(login_id):
    path = os.path.join(STATE_DIR, f"{login_id}.json")
    if not os.path.exists(path):
        return None
    with open(path, encoding="utf-8") as f:
        return json.load(f)

def start(account, password, login_id):
    s = new_session()
    r = s.post(f"{IDMSA}/appleauth/auth/federate?isRememberMeEnabled=true",
               json={"accountName": account, "rememberMe": False}, timeout=30)
    if r.status_code != 200:
        print(json.dumps({"ok": False, "error": f"federate {r.status_code}"}))
        return
    scnt = r.headers.get("scnt", "")
    if scnt: s.headers["scnt"] = scnt
    sid = r.headers.get("X-Apple-ID-Session-Id", "")
    if sid: s.headers["X-Apple-ID-Session-Id"] = sid

    A_b64, a = algo.srp_public()
    r2 = s.post(f"{IDMSA}/appleauth/auth/signin/init",
                json={"a": A_b64, "accountName": account, "protocols": ["s2k", "s2k_fo"]},
                timeout=30)
    if r2.status_code != 200:
        print(json.dumps({"ok": False, "error": f"init {r2.status_code}"}))
        return
    scnt2 = r2.headers.get("scnt", "")
    if scnt2: s.headers["scnt"] = scnt2
    init = r2.json()

    hc_bits = r2.headers.get("X-Apple-HC-Bits")
    hc_challenge = r2.headers.get("X-Apple-HC-Challenge")
    hc_value = None
    if hc_bits and hc_challenge:
        hc_value = algo.hashcash(int(hc_bits), hc_challenge)

    A, M1, M2, K = algo.srp_evidence(
        account, password, init["salt"], init["b"],
        init.get("iteration", 1), init.get("protocol", "s2k"), a=a
    )
    complete_headers = {}
    if hc_value:
        complete_headers["X-Apple-HC"] = hc_value

    r3 = s.post(f"{IDMSA}/appleauth/auth/signin/complete?isRememberMeEnabled=true",
                json={"accountName": account, "rememberMe": True,
                      "m1": M1, "c": init.get("c", ""), "m2": M2, "pause2FA": True},
                timeout=30, headers=complete_headers)

    if r3.status_code == 409 and "hsa2" in r3.text:
        s.get(f"{IDMSA}/appleauth/auth", timeout=30)
        s.put(f"{IDMSA}/appleauth/auth/tdo/securitycode", timeout=30)
        save_state(login_id, s, account)
        print(json.dumps({"ok": True, "status": "need_2fa", "login_id": login_id, "account": account}))
    elif r3.status_code == 200:
        ck = s.cookies.get_dict()
        print(json.dumps({"ok": True, "status": "success", "cookies": ck, "account": account}))
    else:
        body = r3.text[:300]
        if r3.status_code == 401:
            print(json.dumps({"ok": False, "error": "账号或密码错误，或 Apple 暂时拒绝了登录", "status": 401, "body": body}))
        else:
            print(json.dumps({"ok": False, "error": f"complete {r3.status_code}", "body": body}))

def verify(login_id, code):
    state = load_state(login_id)
    if not state:
        print(json.dumps({"ok": False, "error": "login session not found"}))
        return
    s = new_session()
    for k, v in state["cookies"].items():
        s.cookies.set(k, v, domain=".apple.com.cn", path="/")
    for k, v in state["headers"].items():
        if v: s.headers[k] = v

    started = time.perf_counter()
    v = s.post(f"{IDMSA}/appleauth/auth/verify/trusteddevice/securitycode",
               json={"securityCode": {"code": code}}, timeout=30)
    security_code_ms = int((time.perf_counter() - started) * 1000)
    if v.status_code not in (200, 204):
        print(json.dumps({"ok": False, "error": f"verify {v.status_code}",
                          "body": v.text[:120], "account": state.get("account", ""),
                          "steps": {"security_code_ms": security_code_ms}}))
        return

    started = time.perf_counter()
    s.get(f"{IDMSA}/appleauth/auth", timeout=30)
    auth_ms = int((time.perf_counter() - started) * 1000)

    started = time.perf_counter()
    s.get(f"{IDMSA}/appleauth/auth/2sv/trust", timeout=30)
    trust_ms = int((time.perf_counter() - started) * 1000)

    ck = s.cookies.get_dict()
    # Remove state file after success
    path = os.path.join(STATE_DIR, f"{login_id}.json")
    try: os.remove(path)
    except: pass
    print(json.dumps({"ok": True, "status": "success", "cookies": ck,
                      "account": state.get("account", ""),
                      "steps": {"security_code_ms": security_code_ms,
                                "auth_ms": auth_ms, "trust_ms": trust_ms}}))

if __name__ == "__main__":
    cmd = sys.argv[1]
    if cmd == "start":
        start(sys.argv[2], sys.argv[3], sys.argv[4])
    elif cmd == "verify":
        verify(sys.argv[2], sys.argv[3])
