import sys, os, json, subprocess, tempfile, collections
sys.argv=[sys.argv[0],"x"]  # satisfy import-time main guard
src=open('commit_ppr.py').read().replace('\nmain()\n','\n')
exec(compile(src,'commit_ppr','exec'))
tot=collections.Counter()
for name in ["cobra","express","requests","ripgrep","ky","gson","okio","sinatra"]:
    repo=os.path.join(CORP,name); head=git(repo,"rev-parse","HEAD").strip()
    revs=git(repo,"rev-list","--no-merges","-n","400","HEAD").split()
    tmp=tempfile.mkdtemp(prefix="ec-"); git(repo,"worktree","add","--detach",tmp,head); done=0
    for rev in revs:
        if done>=10: break
        parents=git(repo,"rev-list","--parents","-n","1",rev).split()[1:]
        if len(parents)!=1: continue
        parent=parents[0]; msg=git(repo,"log","-1","--format=%s%n%b",rev).strip()
        diff=git(repo,"diff","--name-status","-M",parent,rev).splitlines()
        mod=[l.split("\t")[-1] for l in diff if l.startswith("M") and src_ext(l.split("\t")[-1])] if False else [l.split("\t")[-1] for l in diff if l.startswith("M") and os.path.splitext(l.split("\t")[-1])[1] in EXT]
        if not 1<=len(mod)<=8 or len(msg)<20 or len(diff)>12: continue
        try: git(tmp,"checkout","-q","--detach",parent)
        except RuntimeError: continue
        files=json.loads(subprocess.run([PROTO,tmp],capture_output=True,text=True).stdout)
        truth=set(mod)&set(files); done+=1
        if len(truth)<2: continue
        E=edges_of(files); cc=cochange(repo,parent)
        for t in truth:
            others=truth-{t}
            tot["files"]+=1
            direct=any(o in E[t] for o in others)
            co=any(o in cc.get(t,{}) for o in others)
            tot["import_use_edge"]+=direct; tot["cochange_edge"]+=co; tot["either"]+=(direct or co); tot["both"]+=(direct and co)
    git(repo,"worktree","remove","--force",tmp,check=False)
print(dict(tot))
f=tot["files"]
print({k: round(v/f,2) for k,v in tot.items() if k!="files"})
