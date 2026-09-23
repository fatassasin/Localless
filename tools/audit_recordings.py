import sqlite3, os

db = os.path.join(os.environ["APPDATA"], "localless", "history.db")
c = sqlite3.connect("file:" + db.replace(os.sep, "/") + "?mode=ro", uri=True)

print("status 分布:")
for r in c.execute("select status, count(*) from history group by status order by 2 desc"):
    print("  ", r)

print("\n有音频存档的行（最近 12 条）:")
q = ("select substr(id,1,8), status, round(duration,1), debug_info, created_at "
     "from history where audio_local_path is not null order by created_at desc limit 12")
for r in c.execute(q):
    print("  ", r)

n = c.execute("select count(*) from history where audio_local_path is not null").fetchone()[0]
tot = c.execute("select count(*) from history").fetchone()[0]
print(f"\n有音频的历史行: {n} / 总行数 {tot}")

d = os.path.join(os.environ["APPDATA"], "localless", "recordings")
files = sorted(os.listdir(d), key=lambda f: os.path.getmtime(os.path.join(d, f)))
print(f"\nrecordings/ 共 {len(files)} 个文件，按修改时间:")
import datetime
for f in files:
    p = os.path.join(d, f)
    ts = datetime.datetime.fromtimestamp(os.path.getmtime(p)).strftime("%m-%d %H:%M")
    print(f"   {ts}  {os.path.getsize(p):>9,} B  {f}")
