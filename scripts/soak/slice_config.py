import json,sys,shutil
src=sys.argv[1]; n=int(sys.argv[2])
c=json.load(open(src,encoding='utf-8'))
c["worksets"]=c["worksets"][:n]
json.dump(c,open(src,'w',encoding='utf-8'),ensure_ascii=False,indent=1)
print(f"config now has {len(c['worksets'])} worksets")
