#!/usr/bin/env python3
"""Stdlib-only validator for knowledge-document contracts."""
from __future__ import annotations
import argparse, re, sys, tempfile, unittest
from collections import defaultdict
from datetime import date
from pathlib import Path
from urllib.parse import urlparse

REQUIRED = ('id','kind','status','scope','read_when','last_verified','sources')
KINDS = {'canonical','adapter','index','ledger','plan','runbook','reference'}
STATUSES = {'active','historical','parked','superseded','draft','archived'}
SCOPES = {'repo','repository','project','user','private','public','cross-project'}
PRIVACY = {'public','repo-public','private','project-private','user-private','tooling-private','other-project','internal'}
TREATMENTS = {'migrated','adapted','linked','deferred','retired','excluded','verified','merge','retain-private','summarize','supersede','duplicate','park','split'}
ID = re.compile(r'^[A-Za-z0-9][A-Za-z0-9._/-]*$')
LEDGER_IDS = {f'SRC-{n:03d}' for n in range(1, 59)}
LINK = re.compile(r'!?\[[^]]*\]\(([^)]+)\)')
SKIP = {'.agent-local','.omo','.git','.build','target','.swiftpm','node_modules','dist'}
MACHINE = [
    re.compile(r'/Users/[^\s`"\']+'),
    re.compile(r'/home/[^\s`"\']+'),
    re.compile(r'[A-Za-z]:\\Users\\[^\s`"\']+'),
    re.compile(r'(?<![\w/:])/(?:workspaces?)/[^\s`"\']+'),
    re.compile(r'(?<![\w])~/(?:side-project|Projects|workspace)(?:/|$)[^\s`"\']*', re.I),
]
ROOT_BUILD_FILES = {'Cargo.lock','Cargo.toml','Dockerfile','Justfile','Makefile','Package.resolved','Package.swift','README.md'}
CREDENTIAL_STORAGE = re.compile(
    r'(?i)(?:\b(?:macOS\s+)?keychain\b|\bkeyring\b|\bvault\b|'
    r'\bcredential\s+(?:store|storage)\b|'
    r'(?<![\w])/(?:Users|home)/[^\s`"\']+|'
    r'(?<![\w])~/(?:[^\s`"\']+)|'
    r'(?<![\w])[A-Za-z]:\\Users\\[^\s`"\']+)'
)
SECRET = re.compile(r'(?i)\b(?:api[_-]?key|access[_-]?token|secret|password|private[_-]?key)\s*[:=]\s*(?!\$\{|\{\{|<)[^\s`"\']+')
SENSITIVE = re.compile(r'(?i)(?:token|secret|password|credential|private key|home directory|machine path)')

class Issue:
    def __init__(self,path,line,message): self.path,self.line,self.message=path,line,message
    def __str__(self): return f'{self.path}:{self.line}: {self.message}'

def parse_scalar(v):
    v=v.strip()
    if v.startswith('[') and v.endswith(']'): return [x.strip().strip("'\"") for x in v[1:-1].split(',') if x.strip()]
    if v.startswith('{') and v.endswith('}'):
        result={}
        for item in v[1:-1].split(','):
            if ':' not in item: continue
            key,value=item.split(':',1); result[key.strip().strip("'\"")]=value.strip().strip("'\"")
        return result
    return v.strip("'\"")

def fm(text):
    if not text.startswith('---\n'): return {}, ['missing YAML frontmatter']
    end=text.find('\n---',4)
    if end<0: return {}, ['unterminated YAML frontmatter']
    out={}; errors=[]; active_list=None
    for n,line in enumerate(text[4:end].splitlines(),2):
        if not line.strip() or line.lstrip().startswith('#'): continue
        item=re.match(r'^\s*-\s+(.+)$',line)
        if item and active_list:
            out[active_list].append(parse_scalar(item.group(1))); continue
        m=re.match(r'^([A-Za-z][\w-]*)\s*:\s*(.*)$',line)
        if not m: errors.append(f'invalid frontmatter line {n}; use inline lists or indented - items'); active_list=None; continue
        key,val=m.groups(); val=val.strip()
        if not val: out[key]=[]; active_list=key
        else: out[key]=parse_scalar(val); active_list=None
    return out,errors

def slug(s): return re.sub(r'[-\s]+','-',re.sub(r'[^\w\s-]','',s.lower())).strip('-')
def line_no(text,pos): return text.count('\n',0,pos)+1
def external(t): return bool(urlparse(t).scheme or t.startswith('//'))
def valid_date(value):
    return isinstance(value, str) and bool(re.fullmatch(r'\d{4}-\d{2}-\d{2}', value)) and _date_parses(value)
def _date_parses(value):
    try: date.fromisoformat(value)
    except ValueError: return False
    return True
def path_shaped_source(value):
    if not isinstance(value, str) or not value or re.search(r'\s', value) or external(value) or value.startswith('~/'):
        return False
    return '/' in value or bool(re.search(r'\.[A-Za-z0-9][A-Za-z0-9._-]*$', value)) or value in ROOT_BUILD_FILES
def source_path_issue(root,value):
    if not path_shaped_source(value): return None
    candidate=Path(value)
    if candidate.is_absolute(): return 'source path must be relative to repository root'
    resolved=(root/candidate).resolve()
    try: resolved.relative_to(root)
    except ValueError: return 'source path escapes repository root'
    if not resolved.exists():
        return f'source path does not exist: {value}'
    return None
def relative_links(root,p,text):
    for m in LINK.finditer(text):
        target=m.group(1).strip().split()[0].strip('<>"'); number=line_no(text,m.start())
        if target.startswith('#'): yield p,number,target; continue
        path,_,anchor=target.partition('#')
        if external(path): continue
        resolved=(p.parent/path).resolve()
        try: resolved.relative_to(root)
        except ValueError: yield None,number,'LINK_ESCAPES'; continue
        yield resolved,number,anchor

def validate(root):
    root=Path(root).resolve(); errors=[]
    files=sorted(p for p in root.rglob('*.md') if not (set(p.relative_to(root).parts)&SKIP))
    required_adapters = [root/'AGENTS.md', root/'CLAUDE.md', root/'vendor'/'AGENTS.md', root/'landing'/'AGENTS.md']
    for p in required_adapters:
        label=p.relative_to(root)
        if not p.exists(): errors.append(Issue(label,1,'required adapter file is missing'))
        elif p.is_symlink(): errors.append(Issue(label,1,'must be a regular file, not a symlink'))
    knowledge=root/'docs'/'knowledge'; docs=[p for p in files if knowledge in p.parents]
    root_readme=root/'README.md'; vendor_readme=root/'vendor'/'README.md'
    if not root_readme.exists(): errors.append(Issue('README.md',1,'root README.md is missing'))
    elif not any(t==knowledge/'README.md' for t,_,_ in relative_links(root,root_readme,root_readme.read_text(encoding='utf-8')) if isinstance(t,Path)): errors.append(Issue('README.md',1,'root README.md must link to docs/knowledge/README.md'))
    if vendor_readme.exists() and not any(t==knowledge/'vendor-tokscale.md' for t,_,_ in relative_links(root,vendor_readme,vendor_readme.read_text(encoding='utf-8')) if isinstance(t,Path)): errors.append(Issue('vendor/README.md',1,'vendor README must link to docs/knowledge/vendor-tokscale.md'))
    adapter_paths = {p for p in required_adapters if p.exists() and not p.is_symlink()}
    if not docs: return errors+[Issue('docs/knowledge',1,'knowledge tree is missing')]
    # The pin owner is held to the same frontmatter, link and privacy contract
    # as the documents that restate it; it used to be the least-checked file.
    if vendor_readme.exists(): docs.append(vendor_readme)
    ids={}; meta={}; anchors={}; incoming=defaultdict(set)
    for p in docs:
        rel=p.relative_to(root); text=p.read_text(encoding='utf-8'); data,ferr=fm(text); meta[p]=data
        errors += [Issue(rel,1,e) for e in ferr]
        for key in REQUIRED:
            if not data.get(key): errors.append(Issue(rel,1,f'frontmatter missing required field {key}'))
        if not valid_date(data.get('last_verified')):
            errors.append(Issue(rel,1,'last_verified must be a valid YYYY-MM-DD date'))
        sources=data.get('sources')
        if not isinstance(sources,list) or not sources:
            errors.append(Issue(rel,1,'sources must be a non-empty list'))
        else:
            for source in sources:
                if issue := source_path_issue(root,source):
                    errors.append(Issue(rel,1,issue))
        if data.get('kind') not in KINDS: errors.append(Issue(rel,1,f'invalid kind {data.get("kind")!r}'))
        if data.get('status') not in STATUSES: errors.append(Issue(rel,1,f'invalid status {data.get("status")!r}'))
        if data.get('scope') not in SCOPES: errors.append(Issue(rel,1,f'invalid scope {data.get("scope")!r}'))
        if data.get('privacy') and data['privacy'] not in PRIVACY: errors.append(Issue(rel,1,f'invalid privacy {data["privacy"]!r}'))
        if data.get('scope')=='private' and not data.get('privacy'): errors.append(Issue(rel,1,'private scope requires privacy field'))
        ident=data.get('id')
        if not isinstance(ident,str) or not ID.fullmatch(ident): errors.append(Issue(rel,1,'id must be opaque and repository-safe'))
        elif ident in ids: errors.append(Issue(rel,1,f'duplicate id {ident!r}; already in {ids[ident]}'))
        else: ids[ident]=rel
        anchors[p]={slug(m.group(1)) for m in re.finditer(r'^#{1,6}\s+(.+?)\s*#*\s*$',text,re.M)}
        if data.get('kind')=='plan' and data.get('status')=='superseded':
            for key in ('superseded_by','superseded_on'):
                if not data.get(key): errors.append(Issue(rel,1,f'superseded plan requires {key}'))
        for target,number,anchor in relative_links(root,p,text):
            if target is None: errors.append(Issue(rel,number,'link escapes repository')); continue
            if target==p and anchor and slug(anchor) not in anchors[p]: errors.append(Issue(rel,number,f'missing heading anchor #{anchor}'))
            elif isinstance(target,Path):
                if not target.exists(): errors.append(Issue(rel,number,f'missing link target {target.relative_to(root)}')); continue
                incoming[target].add(p)
                if anchor and target.suffix=='.md':
                    aset={slug(x.group(1)) for x in re.finditer(r'^#{1,6}\s+(.+?)\s*#*\s*$',target.read_text(encoding='utf-8'),re.M)}
                    if slug(anchor) not in aset: errors.append(Issue(rel,number,f'missing heading anchor #{anchor} in {target.relative_to(root)}'))
        errors += scan_text(rel,text)
        if data.get('kind') == 'ledger' and 'source_total' in data:
            check_ledger(root,p,meta,errors)
    # Adapters are part of the same contract, not just knowledge documents.
    adapter_paths.update(p for p in files if p.name in ('AGENTS.md','CLAUDE.md','CONTRIBUTING.md'))
    for p in sorted(adapter_paths): errors += check_adapter(root,p)
    claude=root/'CLAUDE.md'
    if claude.exists() and not claude.is_symlink() and not re.search(r'(?:AGENTS\.md|docs/knowledge(?:/README\.md)?)',claude.read_text(encoding='utf-8'),re.I):
        errors.append(Issue('CLAUDE.md',1,'root CLAUDE.md must route to AGENTS.md or the knowledge index'))
    # Canonical docs (except the index) must be reachable from README/routing descendants.
    routing=knowledge/'README.md'; reachable={routing}; changed=True
    while changed:
        changed=False
        for p in list(reachable):
            if not p.exists(): continue
            for target,_,_ in relative_links(root,p,p.read_text(encoding='utf-8')):
                if isinstance(target,Path) and target.is_file() and target.suffix=='.md' and target not in reachable: reachable.add(target); changed=True
    for p,d in meta.items():
        if d.get('kind')=='canonical' and p!=routing and p not in reachable: errors.append(Issue(p.relative_to(root),1,'orphan canonical document; not reachable from docs/knowledge/README.md'))
    owners=defaultdict(list)
    for p,d in meta.items():
        vals=d.get('canonical_for',[]); vals=vals if isinstance(vals,list) else [vals]
        for value in vals:
            owners[value].append(p)
            label=str(value).lower()
            if 'ledger' in label and any(term in label for term in ('vendor','engine','upstream')): errors.append(Issue(p.relative_to(root),1,'knowledge docs cannot claim exact shared-engine ledger ownership'))
    for value,paths in owners.items():
        if len(paths)>1: errors.append(Issue(paths[1].relative_to(root),1,f'canonical_for {value!r} is owned by multiple documents'))
    vendor=root/'vendor'/'README.md'; vendor_doc=knowledge/'vendor-tokscale.md'
    if not vendor.exists(): errors.append(Issue('vendor/README.md',1,'vendor README is missing'))
    if not vendor_doc.exists(): errors.append(Issue('docs/knowledge/vendor-tokscale.md',1,'vendor-tokscale.md is missing'))
    elif not any(t==vendor for t,_,_ in relative_links(root,vendor_doc,vendor_doc.read_text(encoding='utf-8')) if isinstance(t,Path)): errors.append(Issue(vendor_doc.relative_to(root),1,'vendor-tokscale.md must link to the vendor document'))
    check_engine_history(root,files,errors)
    check_last_verified_moves(root,docs,errors)
    return errors

ENGINE_UPSTREAM_LEDGER = Path('crates/syrtis_core/UPSTREAM.md')
# Historical provenance links may cite the former fork or upstream paths.
# They are citations, not claims about current ownership, so only the label
# must match the target it points at.
ENGINE_BLOB = re.compile(r'\[([^\]]*)\]\(https://github\.com/[^/]+/(?:tokscale-core|tokscale)/blob/([0-9a-f]{40})/[^)]*\)')
SHORT_SHA = re.compile(r'\b[0-9a-f]{7,40}\b')


def check_engine_history(root,files,errors):
    """Historical provenance stays checkable without submodule git access.

    The inherited UPSTREAM.md ledger is provenance, not a release gate: what
    is checked is that engine-link labels name the revision they point at
    (a reader who copies the label audits whatever it says) and that the
    ledger file itself is present. Existence of a cited SHA inside a
    submodule object database is no longer required and no longer checked.
    """
    if not (root/ENGINE_UPSTREAM_LEDGER).is_file():
        errors.append(Issue(str(ENGINE_UPSTREAM_LEDGER),1,'inherited upstream ledger is missing'))
    for p in files:
        rel=p.relative_to(root); text=p.read_text(encoding='utf-8')
        for m in ENGINE_BLOB.finditer(text):
            label,target=m.group(1),m.group(2)
            marks=SHORT_SHA.findall(label)
            if marks and not any(target.startswith(s) for s in marks):
                errors.append(Issue(rel,line_no(text,m.start()),f'engine link label does not match its target\n    label  {marks[0]}\n    target {target}'))


def _toplevel(root):
    import subprocess
    try:
        top=subprocess.run(['git','rev-parse','--show-toplevel'],cwd=root,capture_output=True,text=True,timeout=20)
    except (OSError,subprocess.SubprocessError): return None
    return Path(top.stdout.strip()).resolve() if top.returncode==0 else None

_TOPLEVEL={}
def _git(root,*args):
    """Run git in the superproject. None when git cannot be asked at all.

    A fixture directory nested inside some other checkout must not answer with
    that checkout's history, so the checked root has to be the repository top.
    """
    import subprocess
    key=Path(root).resolve()
    if key not in _TOPLEVEL: _TOPLEVEL[key]=_toplevel(key)
    if _TOPLEVEL[key]!=key: return None
    try: return subprocess.run(['git',*args],cwd=root,capture_output=True,text=True,timeout=20)
    except (OSError,subprocess.SubprocessError): return None

def _ok(out): return out is not None and out.returncode==0

def _merge_base(root):
    mb=_git(root,'merge-base','origin/main','HEAD')
    return mb.stdout.strip() if _ok(mb) and mb.stdout.strip() else None

def _evidence(text):
    """What a verification date vouches for: the body and the sources list."""
    sources=fm(text)[0].get('sources')
    if not text.startswith('---\n'): return sources,text
    end=text.find('\n---',4)
    return sources,(text if end<0 else text[end+4:])

def check_last_verified_moves(root,docs,errors):
    """A document whose body or sources changed must move `last_verified`. Item 9 of #291.

    Compared against the merge base with origin/main, so only this branch's
    edits count; skipped when that base cannot be resolved (no repository, no
    origin/main, or a shallow clone). A new document has no base to compare.

    A date that is already current cannot move further, so a base value on or
    after the merge base's own commit date (less one day, because the date is
    written in the author's timezone and commits carry others) counts as
    fresh. Tied to the tree, not to the day the check runs, so a rerun of the
    same commit gives the same answer.
    """
    base=_merge_base(root)
    if not base: return
    stamp=_git(root,'show','-s','--format=%cs',base)
    if not _ok(stamp) or not valid_date(stamp.stdout.strip()): return
    from datetime import timedelta
    fresh_from=date.fromisoformat(stamp.stdout.strip())-timedelta(days=1)
    rels={p.relative_to(root).as_posix():p for p in docs}
    changed=_git(root,'diff','--name-only',base,'--',*rels)
    if not _ok(changed): return
    for rel in changed.stdout.split():
        p=rels.get(rel)
        if p is None or not p.exists(): continue
        old=_git(root,'show',f'{base}:{rel}')
        if not _ok(old): continue
        new=p.read_text(encoding='utf-8')
        if _evidence(old.stdout)==_evidence(new): continue
        before=fm(old.stdout)[0].get('last_verified'); after=fm(new)[0].get('last_verified')
        fresh=valid_date(before) and date.fromisoformat(before)>=fresh_from
        if before and before==after and not fresh:
            errors.append(Issue(rel,1,f'body or sources changed since {base[:8]} but last_verified is still {after}'))

def check_adapter(root,p):
    text=p.read_text(encoding='utf-8'); rel=p.relative_to(root); out=scan_text(rel,text)
    for target,number,anchor in relative_links(root,p,text):
        if target is None: out.append(Issue(rel,number,'link escapes repository'))
        elif isinstance(target,Path) and not target.exists(): out.append(Issue(rel,number,f'missing adapter link target {target.relative_to(root)}'))
    if not re.search(r'(?:docs/knowledge|canonical|knowledge|AGENTS\.md)',text,re.I): out.append(Issue(rel,1,'adapter does not route to canonical knowledge'))
    return out

def scan_text(rel,text):
    out=[]
    for pattern in MACHINE:
        m=pattern.search(text)
        if m: out.append(Issue(rel,line_no(text,m.start()),'machine-local path is not allowed'))
    for m in SECRET.finditer(text): out.append(Issue(rel,line_no(text,m.start()),'secret value assignment is not allowed'))
    return out

# CI validates the tracked ledger structure; source reconciliation remains a local external audit.
def check_ledger(root,path,meta,errors):
    rel=path.relative_to(root); text=path.read_text(encoding='utf-8'); lines=text.splitlines(); required={'source','kind','topic','status','privacy','treatment','destination','verification'}; header=None; rows=[]; header_line=0
    for n,line in enumerate(lines,1):
        if not line.lstrip().startswith('|'): continue
        cells=[x.strip().lower() for x in line.strip().strip('|').split('|')]
        if required.issubset(cells): header=cells; header_line=n; break
    if header is None: errors.append(Issue(rel,1,'migration ledger table with all required columns was not found')); return
    for n in range(header_line, len(lines)):
        line=lines[n]
        if not line.strip(): break
        if not line.lstrip().startswith('|'): break
        cells=[x.strip() for x in line.strip().strip('|').split('|')]
        if all(re.fullmatch(r':?-+:?',x) for x in cells): continue
        rows.append((n+1,cells))
    if len(rows)!=58: errors.append(Issue(rel,1,f'migration ledger must contain exactly 58 data rows (found {len(rows)})'))
    data=meta.get(path,{})
    if str(data.get('source_total',''))!='58': errors.append(Issue(rel,1,'ledger frontmatter source_total must be 58'))
    counts=data.get('boundary_counts',{})
    parsed_counts=None
    if not isinstance(counts,dict) or not {'memory','plan','local'}.issubset(counts):
        errors.append(Issue(rel,1,'boundary_counts must be a dict containing memory, plan, and local'))
    else:
        try:
            parsed_counts={k:int(counts[k]) for k in ('memory','plan','local')}
        except (TypeError,ValueError):
            errors.append(Issue(rel,1,'boundary_counts values must be integers'))
        if parsed_counts is not None and sum(parsed_counts.values())!=len(rows): errors.append(Issue(rel,1,'boundary_counts total does not equal ledger rows'))
    ix={k:header.index(k) for k in required}; seen=set(); observed=[]; kind_counts=defaultdict(int)
    for n,c in rows:
        if len(c)!=len(header): errors.append(Issue(rel,n,'ledger row column count does not match header')); continue
        raw_source=c[ix['source']].strip(); source=raw_source
        if raw_source.startswith('`') or raw_source.endswith('`'):
            if raw_source.count('`')!=2 or not (raw_source.startswith('`') and raw_source.endswith('`')):
                errors.append(Issue(rel,n,'ledger source id allows at most one pair of backticks'))
            else:
                source=raw_source[1:-1]
        values={k:c[ix[k]].strip() for k in required}; kind=values['kind']; topic=values['topic']; status=values['status']; privacy=values['privacy']; treatment=values['treatment']; destination=values['destination']; verification=values['verification']
        for key,value in values.items():
            if not value: errors.append(Issue(rel,n,f'ledger {key} must not be blank'))
        if not source or source.lower() in seen: errors.append(Issue(rel,n,'ledger source id must be unique and non-blank'))
        seen.add(source.lower()); observed.append(source)
        if not ID.fullmatch(source): errors.append(Issue(rel,n,'ledger source id must be opaque'))
        elif source not in LEDGER_IDS: errors.append(Issue(rel,n,f'ledger source id must be one of SRC-001..SRC-058 (found {source})'))
        if kind not in {'memory','plan','local'}: errors.append(Issue(rel,n,f'invalid ledger kind {kind!r}'))
        else: kind_counts[kind]+=1
        if status not in STATUSES: errors.append(Issue(rel,n,f'invalid ledger status {status!r}'))
        if privacy not in PRIVACY: errors.append(Issue(rel,n,f'invalid ledger privacy {privacy!r}'))
        if treatment not in TREATMENTS: errors.append(Issue(rel,n,f'invalid ledger treatment {treatment!r}'))
        if destination.lower() in ('','tbd','unknown','todo','-'): errors.append(Issue(rel,n,'ledger destination cannot be blank/TBD/unknown'))
        if privacy in {'private','project-private','user-private','tooling-private','other-project'}:
            if re.search(r'(?:/|\\|\.md\b)',source) or re.search(r'(?:/|\\|\.md\b)',topic) or SENSITIVE.search(topic):
                errors.append(Issue(rel,n,'private ledger row exposes filename/path or sensitive topic'))
            for field,value in (('destination',destination),('verification',verification)):
                if CREDENTIAL_STORAGE.search(value):
                    errors.append(Issue(rel,n,f'private ledger row exposes credential storage location in {field}'))
    missing=sorted(LEDGER_IDS-set(observed))
    unexpected=sorted(set(observed)-LEDGER_IDS)
    if missing: errors.append(Issue(rel,1,f'ledger source ids missing expected entries: {", ".join(missing)}'))
    if unexpected: errors.append(Issue(rel,1,f'ledger source ids contain unexpected entries: {", ".join(unexpected)}'))
    if parsed_counts is not None and any(parsed_counts[k]!=kind_counts[k] for k in ('memory','plan','local')):
        errors.append(Issue(rel,1,'boundary_counts do not match ledger row kind counts'))

FIXTURE_PIN='a1b2c3d4e5f60718293a4b5c6d7e8f9012345678'
FIXTURE_OLD_PIN='0f1e2d3c4b5a69788796a5b4c3d2e1f098765432'  # historical-provenance label target only; no pin semantics remain

def self_test():
    class T(unittest.TestCase):
        def root(self,bad=False,parent=None):
            r=Path(tempfile.mkdtemp())
            if parent: r=r/parent/'repo'; r.mkdir(parents=True)
            (r/'AGENTS.md').write_text('See docs/knowledge/README.md'); (r/'CLAUDE.md').write_text('See AGENTS.md'); (r/'vendor').mkdir(); (r/'landing').mkdir(); (r/'README.md').write_text('[Knowledge](docs/knowledge/README.md)'); (r/'vendor/README.md').write_text('---\nid: vendor-readme\nkind: reference\nstatus: active\nscope: repository\nread_when: ownership\nlast_verified: 2026-07-14\nsources: [internal]\n---\n[Knowledge](../docs/knowledge/vendor-tokscale.md)\n'); (r/'vendor/AGENTS.md').write_text('See docs/knowledge/README.md'); (r/'landing/AGENTS.md').write_text('See docs/knowledge/README.md'); k=r/'docs/knowledge'; k.mkdir(parents=True)
            (r/'crates/syrtis_core').mkdir(parents=True)
            (r/'crates/syrtis_core/UPSTREAM.md').write_text('Historical attribution fixture\n')
            rows='\n'.join(f'| `SRC-{i:03d}` | {"local" if i==58 else "plan" if i==57 else "memory"} | topic-{i:03d} | active | public | migrated | doc-{i:03d} | checked |' for i in range(1,59))
            head='---\nid: ledger\nkind: ledger\nstatus: active\nscope: repository\nread_when: migration\nlast_verified: 2026-07-14\nsources: [internal]\nsource_total: 58\nboundary_counts: {memory: 56, plan: 1, local: 1}\n---\n# Ledger\n| source | kind | topic | status | privacy | treatment | destination | verification |\n| --- | --- | --- | --- | --- | --- | --- | --- |\n'
            (k/'ledger.md').write_text(head+rows+'\n\n| verification | result |\n| --- | --- |\n| no-gaps | pass |\n'); (k/'vendor-tokscale.md').write_text('---\nid: vendor\nkind: canonical\nstatus: active\nscope: repo\nread_when: vendor\nlast_verified: 2026-07-14\nsources: [internal]\n---\n# Vendor\n[vendor](../../vendor/README.md)'); (k/'README.md').write_text('---\nid: index\nkind: index\nstatus: active\nscope: repo\nread_when: lookup\nlast_verified: 2026-07-14\nsources: [internal]\n---\n# Index\n[vendor](vendor-tokscale.md)\n[ledger](ledger.md#ledger)')
            if bad: (k/'bad.md').write_text('---\nid: index\nkind: nope\nstatus: active\nscope: wrong\nread_when: lookup\nlast_verified: 2026-07-14\nsources: [internal]\n---\nsecret = sk-live-1 [missing](no.md)')
            return r
        def test_good(self): self.assertEqual(validate(self.root()),[])
        def append_doc(self,r,extra):
            d=r/'docs/knowledge/vendor-tokscale.md'; d.write_text(d.read_text()+extra); return r
        def test_directory_and_binary_links_are_valid_without_markdown_traversal(self):
            r=self.root(); assets=r/'assets'; assets.mkdir()
            image=assets/'chart.png'; image.write_bytes(b'\x89PNG\r\n\x1a\n')
            self.append_doc(r,'\n\n[Fixtures](../../assets)\n[Chart](../../assets/chart.png)\n')
            self.assertEqual(validate(r),[])
            image.unlink(); assets.rmdir()
            errors='\n'.join(map(str,validate(r)))
            self.assertIn('missing link target assets',errors)
            self.assertIn('missing link target assets/chart.png',errors)
        def test_engine_blob_link_labelled_with_its_revision_is_exempt(self):
            self.assertEqual(validate(self.append_doc(self.root(),f'\n\n[`UPSTREAM.md` at `{FIXTURE_OLD_PIN[:7]}`](https://github.com/owner/tokscale-core/blob/{FIXTURE_OLD_PIN}/UPSTREAM.md)\n')),[])
        def test_engine_blob_link_without_a_label_passes(self):
            self.assertEqual(validate(self.append_doc(self.root(),f'\n\n[`UPSTREAM.md`](https://github.com/owner/tokscale/blob/{FIXTURE_PIN}/UPSTREAM.md)\n')),[])
        def test_vendor_readme_is_held_to_the_frontmatter_contract(self):
            r=self.root(); v=r/'vendor/README.md'; v.write_text(v.read_text().split('---\n',2)[2])
            self.assertIn('vendor/README.md:1: missing YAML frontmatter','\n'.join(map(str,validate(r))))
        def git_repo(self,r):
            import subprocess
            def git(*a): return subprocess.run(['git','-c','user.email=t@t','-c','user.name=t',*a],cwd=r,capture_output=True,text=True,check=True).stdout.strip()
            git('init','-q'); return git
        def test_body_change_must_move_last_verified(self):
            r=self.root(); git=self.git_repo(r); git('add','-A'); git('commit','-q','-m','base'); git('update-ref','refs/remotes/origin/main','HEAD')
            d=r/'docs/knowledge/vendor-tokscale.md'; original=d.read_text()
            d.write_text(original.replace('read_when: vendor','read_when: vendor work'))
            self.assertEqual(validate(r),[],'a change to a field that vouches for nothing is not an evidence change')
            d.write_text(original.replace('sources: [internal]','sources: [internal, more]'))
            self.assertIn('body or sources changed since','\n'.join(map(str,validate(r))),'changing the evidence list must move the date')
            d.write_text(original+'\nA new sentence.\n')
            self.assertIn('body or sources changed since','\n'.join(map(str,validate(r))))
            d.write_text(original.replace('last_verified: 2026-07-14','last_verified: 2026-07-15')+'\nA new sentence.\n')
            self.assertEqual(validate(r),[])
        def test_last_verified_is_not_checked_without_a_base(self):
            r=self.root(); git=self.git_repo(r); git('add','-A'); git('commit','-q','-m','base')
            d=r/'docs/knowledge/vendor-tokscale.md'; d.write_text(d.read_text()+'\nA new sentence.\n')
            self.assertEqual(validate(r),[],'no origin/main means cannot ask, not a failure')
        def test_fresh_date_is_tied_to_the_base_commit(self):
            import os
            for stamp,expect_error in (('2026-07-15T12:00:00Z',False),('2026-08-01T12:00:00Z',True)):
                with self.subTest(stamp=stamp):
                    r=self.root(); git=self.git_repo(r); git('add','-A')
                    env=dict(os.environ,GIT_COMMITTER_DATE=stamp,GIT_AUTHOR_DATE=stamp)
                    import subprocess
                    subprocess.run(['git','-c','user.email=t@t','-c','user.name=t','commit','-q','-m','base'],cwd=r,env=env,check=True,capture_output=True)
                    git('update-ref','refs/remotes/origin/main','HEAD')
                    d=r/'docs/knowledge/vendor-tokscale.md'; d.write_text(d.read_text()+'\nA new sentence.\n')
                    found='body or sources changed since' in '\n'.join(map(str,validate(r)))
                    self.assertEqual(found,expect_error,'2026-07-14 is fresh against a base dated 07-15 and stale against 08-01')
        def test_bad(self):
            s='\n'.join(map(str,validate(self.root(True)))); self.assertIn('duplicate id',s); self.assertIn('missing link target',s); self.assertIn('secret value',s); self.assertIn('invalid scope',s)
        def test_ignored_overlay_is_ignored(self):
            for name in ('.agent-local', '.omo'):
                with self.subTest(name=name):
                    r=self.root(); overlay=r/name; nested=overlay/'nested'; nested.mkdir(parents=True)
                    payload='secret = sk-live-1\n/Users/alice/private-notes.md\n[missing](nope.md)'
                    (overlay/'AGENTS.md').write_text(payload); (overlay/'CLAUDE.md').write_text(payload); (nested/'notes.md').write_text(payload)
                    self.assertEqual(validate(r),[])
        def test_owned_engine_docs_are_checked(self):
            r=self.root(); engine=r/'crates/syrtis_core'
            (engine/'AGENTS.md').write_text('secret = sk-live-1\n[missing](nope.md)')
            self.assertIn('secret value','\n'.join(map(str,validate(r))))
        def test_absolute_source_path(self):
            r=self.root(); inside=r/'inside.md'; inside.write_text('fixture'); p=r/'docs/knowledge/ledger.md'; p.write_text(p.read_text().replace('sources: [internal]',f'sources: [{inside}]')); result='\n'.join(map(str,validate(r))); self.assertIn('source path must be relative',result)
        def test_source_path_escape(self):
            r=self.root(); outside=r.parent/f'{r.name}-outside.md'; outside.write_text('fixture'); p=r/'docs/knowledge/ledger.md'; p.write_text(p.read_text().replace('sources: [internal]',f'sources: [../{outside.name}]')); result='\n'.join(map(str,validate(r))); self.assertIn('source path escapes repository root',result); outside.unlink()
        def test_exact_ledger_ids(self):
            r=self.root(); p=r/'docs/knowledge/ledger.md'; p.write_text(p.read_text().replace('`SRC-001`','`SRC-999`',1)); result='\n'.join(map(str,validate(r))); self.assertIn('unexpected entries',result); self.assertIn('SRC-001',result)
        def test_private_credential_location(self):
            r=self.root(); p=r/'docs/knowledge/ledger.md'; text=p.read_text().replace('public | migrated','project-private | migrated',1).replace('checked |','Credentials are stored in the macOS Keychain account named production-token |',1); p.write_text(text); result='\n'.join(map(str,validate(r))); self.assertIn('credential storage location',result)
        def test_private_credential_storage_variant(self):
            r=self.root(); p=r/'docs/knowledge/ledger.md'; text=p.read_text().replace('public | migrated','project-private | migrated',1).replace('checked |','Retrieval uses a vault at /home/alice/.config/tokens |',1); p.write_text(text); result='\n'.join(map(str,validate(r))); self.assertIn('credential storage location',result)
        def test_product_tilde_source(self):
            r=self.root(); p=r/'docs/knowledge/ledger.md'; p.write_text(p.read_text().replace('sources: [internal]','sources: [~/.hermes/profiles/<profile>/state.db]')); self.assertEqual(validate(r),[])
        def test_local_workspace_tilde_path(self):
            r=self.root(); p=r/'docs/knowledge/ledger.md'; p.write_text(p.read_text().replace('topic-001','~/side-project/notes')); result='\n'.join(map(str,validate(r))); self.assertIn('machine-local path',result)
        def test_malformed_boundary_count(self):
            r=self.root(); p=r/'docs/knowledge/ledger.md'; p.write_text(p.read_text().replace('memory: 56','memory: nope')); result='\n'.join(map(str,validate(r))); self.assertIn('boundary_counts values must be integers',result)
    return 0 if unittest.TextTestRunner(verbosity=1).run(unittest.defaultTestLoader.loadTestsFromTestCase(T)).wasSuccessful() else 1

def main(argv=None):
    a=argparse.ArgumentParser(); a.add_argument('--root',type=Path,default=Path(__file__).resolve().parents[1]); a.add_argument('--self-test',action='store_true'); x=a.parse_args(argv)
    if x.self_test: return self_test()
    errors=validate(x.root)
    if errors: print(*errors,sep='\n',file=sys.stderr); print(f'knowledge check failed: {len(errors)} error(s)',file=sys.stderr); return 1
    print('knowledge check passed'); return 0
if __name__=='__main__': raise SystemExit(main())
