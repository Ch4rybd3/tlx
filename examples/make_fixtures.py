#!/usr/bin/env python3
"""Generate sample timelines for manual testing of TLX."""
import csv, random, sys, datetime, os
out = sys.argv[1] if len(sys.argv) > 1 else "fixtures"
big = int(sys.argv[2]) if len(sys.argv) > 2 else 1_000_000
os.makedirs(out, exist_ok=True)
random.seed(42)
start = datetime.datetime(2024, 3, 1, 8, 0, 0)
users = ["bob", "alice", "SYSTEM", "admin", "svc_backup"]
evts = [(4624, "An account was successfully logged on"), (4625, "An account failed to log on"),
        (4688, "A new process has been created"), (7045, "A service was installed in the system"),
        (1102, "The audit log was cleared"), (4720, "A user account was created")]
procs = [r"C:\Windows\System32\cmd.exe", r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
         r"C:\Users\bob\AppData\Local\Temp\evil.exe", r"C:\Windows\explorer.exe"]
def evtx(path, n):
    with open(path, "w", newline="", encoding="utf-8") as f:
        w = csv.writer(f)
        w.writerow(["RecordNumber", "TimeCreated", "EventId", "Level", "Provider", "Channel", "Computer",
                    "UserName", "MapDescription", "PayloadData1", "PayloadData2", "ExecutableInfo", "Payload", "SourceFile"])
        t = start
        for i in range(n):
            t += datetime.timedelta(seconds=random.randint(0, 40), microseconds=random.randint(0, 999999))
            eid, desc = random.choice(evts)
            u = random.choice(users)
            p = random.choice(procs) if eid == 4688 else ""
            w.writerow([i + 1, t.strftime("%Y-%m-%d %H:%M:%S.%f") + "0", eid, random.choice(["Info", "Warning", "Error"]),
                        "Microsoft-Windows-Security-Auditing", "Security", "WKS-0%d" % random.randint(1, 4), u, desc,
                        f"Target: {u}", f"LogonType {random.choice([2,3,10])}" if eid in (4624, 4625) else "", p,
                        '{"EventData": {"Data": [{"@Name": "SubjectUserName", "#text": "%s"}]}}' % u,
                        r"C:\Windows\System32\winevt\Logs\Security.evtx"])
evtx(os.path.join(out, "EvtxECmd_Output.csv"), 5000)
with open(os.path.join(out, "plaso_l2t.csv"), "w", newline="") as f:
    w = csv.writer(f)
    w.writerow("date,time,timezone,MACB,source,sourcetype,type,user,host,short,desc,version,filename,inode,notes,format,extra".split(","))
    srcs = [("FILE", "NTFS MFT", "Creation Time"), ("REG", "Registry Key", "Last Written"), ("EVT", "WinEVTX", "Content Modification Time"),
            ("WEBHIST", "Chrome History", "Last Visited Time"), ("LNK", "Windows Shortcut", "Last Access"), ("PE", "PE Compilation time", "Creation Time"),
            ("LOG", "Syslog", "Content Modification Time"), ("RECBIN", "Recycle Bin", "Content Deletion Time")]
    t = start
    for i in range(3000):
        t += datetime.timedelta(seconds=random.randint(0, 120))
        s, st, ty = random.choice(srcs)
        w.writerow([t.strftime("%m/%d/%Y"), t.strftime("%H:%M:%S"), "UTC", random.choice(["M...", ".A..", "..C.", "...B", "MACB"]), s, st, ty,
                    random.choice(users), "WKS-01", f"{st} entry {i}", f"Long description for {st} event number {i}", 2, f"/evidence/{s.lower()}/{i}.bin", i * 7, "-", "plaso", "sha256_hash: abc"])
evtx(os.path.join(out, "big.csv"), big)
print("ok")
