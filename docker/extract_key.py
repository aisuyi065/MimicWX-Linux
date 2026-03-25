#!/usr/bin/env python3
"""
微信数据库密钥提取脚本 (内存扫描 + salt 匹配)

原理:
  WCDB 在进程内存中缓存 x'<64hex_enc_key><32hex_salt>'
  每个加密数据库的 page 1 前 16 字节是 salt
  通过比对 salt 将密钥匹配到正确的数据库
  用 HMAC-SHA512 验证密钥正确性
"""

import re, os, sys, time, json, struct, hashlib
import hmac as hmac_mod

# 持久化路径 (避免 docker restart 丢失)
KEY_DIR = "/home/wechat/.xwechat"
KEY_FILE = os.path.join(KEY_DIR, "wechat_key.txt")
KEY_JSON_FILE = os.path.join(KEY_DIR, "wechat_keys.json")
# 兼容旧路径 (MimicWX 可能从 /tmp 读取)
KEY_FILE_COMPAT = "/tmp/wechat_key.txt"
KEY_JSON_COMPAT = "/tmp/wechat_keys.json"
KEY_PATTERN = rb"x'([0-9a-fA-F]{96})'"
SCAN_INTERVAL = 3
MAX_WAIT = 300

PAGE_SZ = 4096
KEY_SZ = 32
SALT_SZ = 16
IV_SZ = 16
HMAC_SZ = 64
RESERVE_SZ = 80

def find_wechat_pid():
    for p in os.listdir('/proc'):
        try:
            pid = int(p)
            with open(f'/proc/{pid}/comm', 'r') as f:
                if f.read().strip() == 'wechat':
                    return pid
        except:
            pass
    return None

def scan_process_memory(pid):
    keys = []
    try:
        with open(f"/proc/{pid}/maps", 'r') as f:
            regions = f.readlines()
        mem_fd = os.open(f"/proc/{pid}/mem", os.O_RDONLY)
    except:
        return keys

    for region in regions:
        parts = region.split()
        if len(parts) < 2 or 'r' not in parts[1]:
            continue
        if len(parts) >= 6 and '/' in parts[5].strip() and not parts[5].strip().startswith('['):
            continue
        addr_range = parts[0].split('-')
        start, end = int(addr_range[0], 16), int(addr_range[1], 16)
        if end - start > 100 * 1024 * 1024:
            continue
        try:
            os.lseek(mem_fd, start, os.SEEK_SET)
            data = os.read(mem_fd, end - start)
            for m in re.finditer(KEY_PATTERN, data):
                hex_str = m.group(1).decode()
                keys.append({
                    'enc_key': hex_str[:64],
                    'salt': hex_str[64:],
                    'raw_key': hex_str,
                })
        except:
            pass
    os.close(mem_fd)
    return keys

def find_db_dir():
    """查找微信数据库目录"""
    base = "/home/wechat/Documents/xwechat_files"
    if not os.path.exists(base):
        return None
    for d in os.listdir(base):
        db_dir = os.path.join(base, d, "db_storage")
        if os.path.exists(db_dir):
            return db_dir
    return None

def derive_mac_key(enc_key_bytes, salt_bytes):
    """从 enc_key 派生 HMAC 密钥 (和 wechat-decrypt 相同逻辑)"""
    mac_salt = bytes(b ^ 0x3a for b in salt_bytes)
    return hashlib.pbkdf2_hmac("sha512", enc_key_bytes, mac_salt, 2, dklen=KEY_SZ)

def verify_key_for_db(db_path, enc_key_hex):
    """验证密钥是否能解密数据库 (HMAC-SHA512 验证 page 1)"""
    enc_key = bytes.fromhex(enc_key_hex)
    
    try:
        with open(db_path, 'rb') as f:
            page1 = f.read(PAGE_SZ)
    except:
        return False
    
    if len(page1) < PAGE_SZ:
        return False
    
    # page 1 前 16 字节是 salt
    salt = page1[:SALT_SZ]
    mac_key = derive_mac_key(enc_key, salt)
    
    # HMAC 数据: salt 后到 reserve 区的 IV 之后 (即 page[16:4032])
    hmac_data = page1[SALT_SZ : PAGE_SZ - RESERVE_SZ + IV_SZ]
    stored_hmac = page1[PAGE_SZ - HMAC_SZ : PAGE_SZ]
    
    hm = hmac_mod.new(mac_key, hmac_data, hashlib.sha512)
    hm.update(struct.pack('<I', 1))  # page number
    
    return hm.digest() == stored_hmac

def match_keys_to_dbs(keys, db_dir):
    """用 salt 匹配 + HMAC 验证找到每个数据库的正确密钥"""
    db_files = []
    for root, dirs, files in os.walk(db_dir):
        for f in files:
            if f.endswith('.db') and not f.endswith(('-wal', '-shm')):
                rel = os.path.relpath(os.path.join(root, f), db_dir)
                db_files.append(rel)
    
    # 方法1: salt 匹配 (快速)
    salt_map = {}
    for k in keys:
        salt_map[k['salt']] = k
    
    matched = {}
    unmatched_dbs = []
    
    for rel in sorted(db_files):
        db_path = os.path.join(db_dir, rel)
        try:
            with open(db_path, 'rb') as f:
                db_salt = f.read(SALT_SZ).hex()
        except:
            continue
        
        if db_salt in salt_map:
            k = salt_map[db_salt]
            # HMAC 验证
            if verify_key_for_db(db_path, k['enc_key']):
                matched[rel] = k
                print(f"[extract_key]   [ok] {rel} → salt 匹配 + HMAC 验证通过")
            else:
                # salt 匹配但 HMAC 失败，尝试其他密钥
                unmatched_dbs.append(rel)
        else:
            unmatched_dbs.append(rel)
    
    # 方法2: 暴力匹配 (对未匹配的数据库)
    for rel in unmatched_dbs:
        db_path = os.path.join(db_dir, rel)
        for k in keys:
            if verify_key_for_db(db_path, k['enc_key']):
                matched[rel] = k
                print(f"[extract_key]   [ok] {rel} → HMAC 暴力匹配成功")
                break
    
    return matched

def save_keys(matched, all_keys):
    """保存匹配结果 (同时写入持久化路径和兼容路径)"""
    os.makedirs(KEY_DIR, exist_ok=True)
    mapping = {}
    for db, k in matched.items():
        mapping[db] = k['raw_key']
    
    for jpath in [KEY_JSON_FILE, KEY_JSON_COMPAT]:
        with open(jpath, 'w') as f:
            json.dump(mapping, f, indent=2)
    
    if matched:
        first_key = list(matched.values())[0]
        for kpath in [KEY_FILE, KEY_FILE_COMPAT]:
            with open(kpath, 'w') as f:
                f.write(first_key['raw_key'])

def main():
    print("[extract_key] [key] 微信密钥提取脚本启动 (内存扫描 + HMAC 验证)")

    pid = None
    for _ in range(60):
        pid = find_wechat_pid()
        if pid:
            break
        time.sleep(1)
    if not pid:
        print("[extract_key] [err] 未找到微信进程")
        sys.exit(1)

    print(f"[extract_key] 微信 PID: {pid}")
    print("[extract_key] [wait] 等待用户扫码登录...")
    print("[extract_key] [login] 请通过 noVNC (http://localhost:6080/vnc.html) 扫码登录微信")

    start_time = time.time()
    while time.time() - start_time < MAX_WAIT:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            print("[extract_key] [err] 微信进程已退出")
            sys.exit(1)

        keys = scan_process_memory(pid)
        if not keys:
            elapsed = int(time.time() - start_time)
            if elapsed % 30 == 0 and elapsed > 0:
                print(f"[extract_key] [wait] 已等待 {elapsed}s...")
            time.sleep(SCAN_INTERVAL)
            continue

        # 去重
        unique = {}
        for k in keys:
            if k['raw_key'] not in unique:
                unique[k['raw_key']] = k
        keys = list(unique.values())
        
        print(f"[extract_key] 找到 {len(keys)} 个唯一密钥, 开始匹配数据库...")

        db_dir = find_db_dir()
        if not db_dir:
            print("[extract_key] [warn] 数据库目录未就绪, 稍后重试...")
            time.sleep(5)
            continue

        matched = match_keys_to_dbs(keys, db_dir)
        
        if matched:
            save_keys(matched, keys)
            print(f"[extract_key] [ok] 成功匹配 {len(matched)} 个数据库的密钥!")
            print(f"[extract_key] 密钥已保存到 {KEY_FILE} 和 {KEY_JSON_FILE}")
            # 延迟重扫: 微信可能在登录后才创建部分 DB (如 message_0.db)
            rescan_for_new_dbs(pid, db_dir, matched)
            return
        else:
            # 密钥找到但没匹配到数据库 (可能数据库还没创建完)
            elapsed = int(time.time() - start_time)
            if elapsed < 30:
                print(f"[extract_key] [warn] 密钥未匹配到数据库, 等待数据库就绪...")
                time.sleep(5)
                continue
            else:
                # 超过 30 秒还没匹配到, 直接保存
                print(f"[extract_key] [warn] 未匹配到数据库, 保存原始密钥")
                save_keys({}, keys)
                for kpath in [KEY_FILE, KEY_FILE_COMPAT]:
                    with open(kpath, 'w') as f:
                        f.write(keys[0]['raw_key'])
                return

    print(f"[extract_key] [err] 超时 ({MAX_WAIT}s), 未找到密钥")
    sys.exit(1)

def rescan_for_new_dbs(pid, db_dir, initial_matched):
    """延迟重扫: 监控 db_storage 30s, 有新 .db 就重新提取并匹配"""
    initial_dbs = set(initial_matched.keys())
    print(f"[extract_key] [scan] 开始监控新数据库 (30s)...")
    
    for i in range(6):  # 6 x 5s = 30s
        time.sleep(5)
        
        # 检查进程是否存活
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            print("[extract_key] [warn] 微信进程已退出, 停止监控")
            return
        
        # 扫描当前所有 DB
        current_dbs = set()
        for root, dirs, files in os.walk(db_dir):
            for f in files:
                if f.endswith('.db') and not f.endswith(('-wal', '-shm')):
                    rel = os.path.relpath(os.path.join(root, f), db_dir)
                    current_dbs.add(rel)
        
        new_dbs = current_dbs - initial_dbs
        if not new_dbs:
            continue
        
        print(f"[extract_key] [new] 发现 {len(new_dbs)} 个新数据库: {', '.join(sorted(new_dbs))}")
        
        # 重新扫描内存 (新 DB 的密钥可能刚加载)
        keys = scan_process_memory(pid)
        if not keys:
            continue
        
        unique = {}
        for k in keys:
            if k['raw_key'] not in unique:
                unique[k['raw_key']] = k
        keys = list(unique.values())
        
        # 重新匹配所有 DB
        matched = match_keys_to_dbs(keys, db_dir)
        if len(matched) > len(initial_matched):
            save_keys(matched, keys)
            new_count = len(matched) - len(initial_matched)
            print(f"[extract_key] [ok] 更新: 新增 {new_count} 个密钥, 共 {len(matched)} 个")
            initial_matched.update(matched)
            initial_dbs = set(initial_matched.keys())
    
    print(f"[extract_key] [scan] 监控结束, 最终匹配 {len(initial_matched)} 个数据库")


if __name__ == "__main__":
    main()
