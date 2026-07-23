import json,sys,uuid
master=sys.argv[1]; out=sys.argv[2]
c=json.load(open(master,encoding='utf-8'))
main_ids=c["main_monitor_ids"]
# sub-screen ids (動画=one, モニター=two_columns) from the master config
subs={s.get("split"): s["id"] for s in c.get("sub_screens",[])}
sub_one = next((s["id"] for s in c["sub_screens"] if s["split"]=="one"), None)
sub_cols = next((s["id"] for s in c["sub_screens"] if s["split"]=="two_columns"), None)
def sub_policy(sid): return {"sub_screen":{"sub_screen_id":sid}}
CODE=r"C:\Users\zappy\AppData\Local\Programs\Microsoft VS Code\Code.exe"
BRAVE=r"C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe"
CHATGPT=r"C:\Program Files\WindowsApps\OpenAI.Codex_26.715.4045.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe"
CLS="Chrome_WidgetWin_1"
def win(exe,pname,title,idx,z):
    mon=main_ids[idx]
    return {"id":str(uuid.uuid4()),
      "matcher":{"executable_path":exe,"process_name":pname,"window_class":CLS,
                 "registered_title":title,"title_contains":title,"title_regex":None},
      "main_placement":{"monitor_id":mon,"main_monitor_index":idx,
                 "normalized_rect":{"x":0.0,"y":0.0,"width":1.0,"height":1.0},
                 "physical_rect_at_capture":{"x":0,"y":0,"width":1920,"height":1080},
                 "show_state":"maximized"},
      "z_order":z,"launch_spec":None}
def ws(name,wins,sort,repo_path="",kind="directory",parking="auto"):
    return {"id":str(uuid.uuid4()),"name":name,"repository_path":repo_path,
      "repository_kind":kind,"color":"#64B5F6","sort_order":sort,"direct_hotkey":None,
      "parking_policy":parking,"fullscreen_when_parked":False,"windows":wins,
      "created_at":"2026-07-23T00:00:00Z","updated_at":"2026-07-23T00:00:00Z"}
worksets=[]
# index0 = Codex + Brave (always included since slicing takes first N)
worksets.append(ws("SET-01-Codex",
    [win(CHATGPT,"ChatGPT.exe","ChatGPT",0,0), win(BRAVE,"brave.exe","BSET-01",1,1)], 1))
# index1..14 = VSCode + Brave ; repos 01..14 ; 01-07 folder, 08-14 workspace
for i in range(1,15):
    repo=i                      # repo01..repo14
    folder = repo<=7
    vtitle = f"repo{repo:02}" if folder else f"WS repo{repo:02}"
    btitle = f"BSET-{i+1:02}"
    path = rf"C:\code\test\repo{repo:02}"
    kind = "git" if folder else "workspace"
    # Assign the first two VSCode sets to sub-screens so subs are exercised at
    # every soak size (worksets[1]/[2] are always inside the first-N slice).
    parking = "auto"
    if i==1 and sub_one:  parking = sub_policy(sub_one)     # SET-02 -> 動画 (one)
    if i==2 and sub_cols: parking = sub_policy(sub_cols)    # SET-03 -> モニター (two_columns)
    worksets.append(ws(f"SET-{i+1:02}",
        [win(CODE,"Code.exe",vtitle,0,0), win(BRAVE,"brave.exe",btitle,1,1)],
        i+1, path, kind, parking))
c["worksets"]=worksets
json.dump(c,open(out,'w',encoding='utf-8'),ensure_ascii=False,indent=1)
print(f"wrote {out}: {len(worksets)} worksets (0=Codex)")
for w in worksets[:3]:
    print("  ",w["name"],[(m['matcher']['title_contains']) for m in w['windows']],w['repository_kind'])
