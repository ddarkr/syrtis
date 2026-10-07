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
EXTERNAL_DOC_ROOT = ('vendor', 'tokscale-core')
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
        submodule=(root/Path(*EXTERNAL_DOC_ROOT)).resolve()
        gitmodules=root/'.gitmodules'
        try: resolved.relative_to(submodule)
        except ValueError: pass
        else:
            declared=gitmodules.is_file() and re.search(
                r'(?m)^\s*path\s*=\s*vendor/tokscale-core\s*$',
                gitmodules.read_text(encoding='utf-8'),
            )
            uninitialized=not submodule.exists() or (submodule.is_dir() and not any(submodule.iterdir()))
            if declared and uninitialized: return None
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
    files=sorted(p for p in root.rglob('*.md') if not (set(p.relative_to(root).parts)&SKIP) and p.relative_to(root).parts[:2] != EXTERNAL_DOC_ROOT)
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
    if not vendor.exists(): errors.append(Issue('vendor/README.md',1,'consumer pin owner is missing'))
    if not vendor_doc.exists(): errors.append(Issue('docs/knowledge/vendor-tokscale.md',1,'vendor-tokscale.md is missing'))
    elif not any(t==vendor for t,_,_ in relative_links(root,vendor_doc,vendor_doc.read_text(encoding='utf-8')) if isinstance(t,Path)): errors.append(Issue(vendor_doc.relative_to(root),1,'vendor-tokscale.md must link to the consumer pin document'))
    check_engine_pin(root,files,errors)
    check_engine_delta(root,files,errors)
    check_advance_claims(root,files,errors)
    check_last_verified_moves(root,docs,errors)
    ledger=next((p for p,d in meta.items() if d.get('kind')=='ledger'),None)
    if ledger: check_ledger(root,ledger,meta,errors)
    else: errors.append(Issue('docs/knowledge',1,'migration ledger document is missing'))
    return errors

PIN_ROW = re.compile(r'\|\s*Reviewed pin\s*\|\s*`([0-9a-f]{40})`\s*\|')
# The phrase must sit next to the SHA it introduces. A knowledge table row can
# be thousands of characters long and legitimately cite several historical
# engine revisions, so anchoring on the line would flag those as stale.
PIN_CLAIM = re.compile(r'(?i)(?:reviewed\s+pin|現在\s*pin\s+reviewed)[^\n]{0,80}?`([0-9a-f]{40})`')
ENGINE_BLOB = re.compile(r'\[([^\]]*)\]\(https://github\.com/[^/]+/tokscale-core/blob/([0-9a-f]{40})/[^)]*\)')
SHORT_SHA = re.compile(r'\b[0-9a-f]{7,40}\b')
ENGINE_SHA = re.compile(r'https://github\.com/[^/]+/tokscale-core/(?:blob|commit|tree)/([0-9a-f]{40})')

def gitlink_pin(root):
    """The commit the submodule is pinned to right now, or None outside a checkout.

    `vendor/README.md` declares the pin, but a declaration is not the pin --
    the gitlink is. Comparing documents only to each other passes a tree where
    every document agrees on a SHA that was never checked out.

    Reads the submodule's own HEAD rather than `git ls-tree HEAD`, because a
    pin advance edits the gitlink and the documents together and runs this
    before committing. Reading the committed tree would reject every correct
    advance and pass only after the fact, which is the wrong way round for a
    pre-commit gate.
    """
    import subprocess
    engine=root/'vendor'/'tokscale-core'
    if not (engine/'.git').exists(): return None
    try:
        out=subprocess.run(['git','rev-parse','HEAD'],cwd=engine,
                           capture_output=True,text=True,timeout=10)
    except (OSError,subprocess.SubprocessError): return None
    if out.returncode!=0: return None
    m=re.match(r'([0-9a-f]{40})',out.stdout.strip())
    return m.group(1) if m else None

def engine_is_shallow(root):
    """Whether the submodule's object database is truncated.

    CI checks out submodules at depth 1, so it holds the current pin and
    nothing else. Absence there means "not fetched", not "not a commit", and
    the two must not produce the same verdict.
    """
    import subprocess
    engine=root/'vendor'/'tokscale-core'
    try:
        out=subprocess.run(['git','rev-parse','--is-shallow-repository'],cwd=engine,
                           capture_output=True,text=True,timeout=10)
    except (OSError,subprocess.SubprocessError): return True
    return out.returncode!=0 or out.stdout.strip()!='false'

def sha_exists_in_engine(root,sha):
    """Whether a SHA resolves in the submodule.

    None when the question cannot be answered here -- no checkout, or a shallow
    one that would report every historical revision as missing.
    """
    import subprocess
    engine=root/'vendor'/'tokscale-core'
    if not (engine/'.git').exists(): return None
    if engine_is_shallow(root): return None
    try:
        out=subprocess.run(['git','cat-file','-e',f'{sha}^{{commit}}'],cwd=engine,
                           capture_output=True,text=True,timeout=10)
    except (OSError,subprocess.SubprocessError): return None
    return out.returncode==0

UNRESOLVED = object()  # asked and answered no, as distinct from cannot ask
DELTA_CLAIM = re.compile(r'`([0-9a-f]{7,40})`\s*(?:→|->)\s*`([0-9a-f]{7,40})`\s*delta\s*(?:為|is)\s*([0-9,]+)\s*(?:個\s*)?engine\s*commit')

def engine_rev_count(root,base,head):
    """Commits in `base..head` inside the submodule.

    Three outcomes, and collapsing them is how a guard passes while doing
    nothing: an integer is the answer; None means the question cannot be asked
    here (no checkout, or a shallow one that does not hold the range); and
    UNRESOLVED means it was asked and the revisions are not in this engine,
    which is a defect in the claim rather than a limit of the environment.
    """
    import subprocess
    engine=root/'vendor'/'tokscale-core'
    if not (engine/'.git').exists() or engine_is_shallow(root): return None
    try:
        out=subprocess.run(['git','rev-list','--count',f'{base}..{head}'],cwd=engine,
                           capture_output=True,text=True,timeout=10)
    except (OSError,subprocess.SubprocessError): return None
    if out.returncode!=0: return UNRESOLVED
    return int(out.stdout.strip() or 0)

def check_engine_delta(root,files,errors):
    """A delta claim names two revisions and a count; the count is derivable.

    Three advances in a row shipped a stale one -- 52 survived into an advance
    of 10, and 10 into an advance of 7 -- because the prose above it was
    rewritten while the number under it was not. It is the one part of a
    consequence paragraph a script can settle.
    """
    for p in files:
        rel=p.relative_to(root); text=p.read_text(encoding='utf-8')
        for m in DELTA_CLAIM.finditer(text):
            base,head,claimed=m.group(1),m.group(2),int(m.group(3).replace(',',''))
            actual=engine_rev_count(root,base,head)
            if actual is None or actual==claimed: continue
            if actual is UNRESOLVED:
                errors.append(Issue(rel,line_no(text,m.start()),
                    f'delta endpoints do not resolve in the engine: {base}..{head}'))
                continue
            errors.append(Issue(rel,line_no(text,m.start()),
                f'delta count disagrees with the engine\n    claimed {claimed} for {base}..{head}\n    actual  {actual}'))

def check_engine_pin(root,files,errors):
    """The reviewed engine pin is restated across several documents; vendor/README.md owns it.

    Two shapes carry it and both drift. A prose claim names the SHA directly;
    a permalink into the engine embeds it in the URL. Historical revisions are
    legitimately cited in both shapes, so a citation is exempt only when it
    says which revision it means: a link whose text carries the matching short
    SHA is deliberate, a bare link is a claim about the current pin.

    The owner document is checked against the gitlink rather than trusted, and
    every engine SHA is checked for existence -- a fabricated one agrees with
    itself everywhere it is pasted.
    """
    vendor_readme=root/'vendor'/'README.md'
    if not vendor_readme.exists(): return
    row=PIN_ROW.search(vendor_readme.read_text(encoding='utf-8'))
    if not row:
        errors.append(Issue('vendor/README.md',1,'reviewed pin row is missing or malformed'))
        return
    pin=row.group(1)
    gitlink=gitlink_pin(root)
    if gitlink and gitlink!=pin:
        errors.append(Issue('vendor/README.md',1,f'declared pin does not match the gitlink\n    declared {pin}\n    gitlink  {gitlink}'))
    seen=set()
    for p in files:
        rel=p.relative_to(root); text=p.read_text(encoding='utf-8')
        for m in PIN_CLAIM.finditer(text):
            if m.group(1)!=pin: errors.append(Issue(rel,line_no(text,m.start()),f'stale reviewed pin\n    found  {m.group(1)}\n    pin    {pin}'))
        for m in ENGINE_BLOB.finditer(text):
            label,target=m.group(1),m.group(2)
            marks=SHORT_SHA.findall(label)
            if marks:
                # A labelled link cites a specific revision. The label may name a
                # historical one, but it must name the one the URL points at --
                # a reader who copies the label audits whatever it says.
                if not any(target.startswith(s) for s in marks):
                    errors.append(Issue(rel,line_no(text,m.start()),f'engine link label does not match its target\n    label  {marks[0]}\n    target {target}'))
                continue
            if target!=pin: errors.append(Issue(rel,line_no(text,m.start()),f'stale engine link (label a revision explicitly to cite a historical one)\n    found  {target}\n    pin    {pin}'))
        for m in ENGINE_SHA.finditer(text):
            sha=m.group(1)
            if sha in seen: continue
            seen.add(sha)
            if sha_exists_in_engine(root,sha) is False:
                errors.append(Issue(rel,line_no(text,m.start()),f'engine commit does not exist: {sha}'))
    if sha_exists_in_engine(root,pin) is False:
        errors.append(Issue('vendor/README.md',1,f'declared pin does not exist in the engine: {pin}'))

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

ADVANCE_PATHS=('crates/tb_core_ffi','Sources/CTB/include/ctb.h')

def advance_touched(root):
    """Which of ADVANCE_PATHS the current engine pin advance changed.

    None when the question cannot be answered here: not a repository, or a
    shallow clone that does not hold what it needs. The advance is, in order:

    - this branch, when the gitlink (or the submodule checkout, during an
      uncommitted advance) differs from the merge base with origin/main.
      Measured from that merge base to the working tree, so FFI edits made in
      an earlier commit of the same branch count. In CI's pull request merge
      ref the merge base is main, so this is the whole pull request;
    - an uncommitted advance with no usable merge base: HEAD to working tree;
    - otherwise the newest first-parent commit that moved the gitlink,
      against its first parent, which for a merged pull request is the whole
      pull request.
    """
    committed=_git(root,'rev-parse','HEAD:vendor/tokscale-core')
    if not _ok(committed): return None
    current=gitlink_pin(root) or committed.stdout.strip()
    mb=_merge_base(root)
    at_base=_git(root,'rev-parse',f'{mb}:vendor/tokscale-core') if mb else None
    # Either the committed gitlink or the checkout moving counts: a branch that
    # committed an advance while the submodule still sits at the old pin must
    # not fall through to the HEAD range and miss the committed FFI edits.
    base_link=at_base.stdout.strip() if _ok(at_base) else None
    if base_link and (base_link!=current or base_link!=committed.stdout.strip()):
        rng=[mb]
    elif current!=committed.stdout.strip():
        rng=['HEAD']
    else:
        log=_git(root,'log','--first-parent','-1','--format=%H','--','vendor/tokscale-core')
        if not _ok(log) or not log.stdout.strip(): return None
        c=log.stdout.strip()
        if not _ok(_git(root,'rev-parse','--verify','--quiet',f'{c}^1')): return None
        rng=[f'{c}^1',c]
    out=_git(root,'diff','--name-only',*rng,'--',*ADVANCE_PATHS)
    if not _ok(out): return None
    names=out.stdout.split()
    return {p for p in ADVANCE_PATHS if any(n==p or n.startswith(p+'/') for n in names)}

# Only sentences that say they describe the current advance are checked.
# Consequence prose also records earlier advances ("該次", "前次", "That
# advance"), and those claims stay true when the current one changes the
# FFI. A current-advance claim worded without one of these markers escapes
# this check; it is a reminder triggered by the observed diff, not a proof.
# Quoted text is removed first: a sentence that quotes the claim in order to
# scope it (「這次 … `ctb.h` 簽名不變」的敘述只描述 …) is not making it.
CURRENT_ADVANCE=re.compile(r'(?i)這次|本次|this (?:pin )?advance|current pin')
UNCHANGED_CLAIM={
    'crates/tb_core_ffi':re.compile(r'`crates/tb_core_ffi`[^。；;\n]{0,24}?(?:零改動|unchanged|untouched)',re.I),
    'Sources/CTB/include/ctb.h':re.compile(r'`ctb\.h`[^。；;\n]{0,24}?(?:簽名不變|unchanged)',re.I),
}
QUOTED=re.compile(r'「[^」]*」|“[^”]*”|"[^"\n]*"')
# Clause boundaries: CJK sentence ends, semicolons, table cells, an English
# full stop followed by a space, and line ends.
CLAUSE=re.compile(r'[^。！？；;|\n]+?(?:[。！？；;|\n]|\.(?=\s)|$)')

def check_advance_claims(root,files,errors):
    """"This advance leaves the FFI crate / C header unchanged" must match the diff.

    Items 7 and 8 of #291. The claim is copied forward with each pin advance,
    so the one time an advance does touch `crates/tb_core_ffi` or `ctb.h` is
    exactly when the stale sentence survives.
    """
    touched=advance_touched(root)
    if not touched: return
    for p in files:
        rel=p.relative_to(root); text=p.read_text(encoding='utf-8')
        for m in CLAUSE.finditer(text):
            clause=QUOTED.sub('',m.group(0))
            if not CURRENT_ADVANCE.search(clause): continue
            for path in sorted(touched):
                if UNCHANGED_CLAIM[path].search(clause):
                    errors.append(Issue(rel,line_no(text,m.start()),f'claims the current pin advance left {path} unchanged, but the advance changed it'))

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
FIXTURE_OLD_PIN='0f1e2d3c4b5a69788796a5b4c3d2e1f098765432'

def self_test():
    class T(unittest.TestCase):
        def root(self,bad=False,parent=None):
            r=Path(tempfile.mkdtemp())
            if parent: r=r/parent/'repo'; r.mkdir(parents=True)
            (r/'AGENTS.md').write_text('See docs/knowledge/README.md'); (r/'CLAUDE.md').write_text('See AGENTS.md'); (r/'vendor').mkdir(); (r/'landing').mkdir(); (r/'README.md').write_text('[Knowledge](docs/knowledge/README.md)'); (r/'vendor/README.md').write_text(f'---\nid: vendor-readme\nkind: reference\nstatus: active\nscope: repository\nread_when: pin\nlast_verified: 2026-07-14\nsources: [internal]\n---\n[Knowledge](../docs/knowledge/vendor-tokscale.md)\n\n| Field | Value |\n|---|---|\n| Reviewed pin | `{FIXTURE_PIN}` |\n'); (r/'vendor/AGENTS.md').write_text('See docs/knowledge/README.md'); (r/'landing/AGENTS.md').write_text('See docs/knowledge/README.md'); k=r/'docs/knowledge'; k.mkdir(parents=True)
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
        def test_engine_pin_claim_matching_owner_passes(self):
            self.assertEqual(validate(self.append_doc(self.root(),f'\n\nNative reviewed pin is `{FIXTURE_PIN}`.\n')),[])
        def test_stale_engine_pin_claim_is_reported(self):
            self.assertIn('stale reviewed pin','\n'.join(map(str,validate(self.append_doc(self.root(),f'\n\nNative reviewed pin is `{FIXTURE_OLD_PIN}`.\n')))))
        def test_stale_engine_blob_link_is_reported(self):
            self.assertIn('stale engine link','\n'.join(map(str,validate(self.append_doc(self.root(),f'\n\n[`UPSTREAM.md`](https://github.com/owner/tokscale-core/blob/{FIXTURE_OLD_PIN}/UPSTREAM.md)\n')))))
        def test_engine_blob_link_labelled_with_its_revision_is_exempt(self):
            self.assertEqual(validate(self.append_doc(self.root(),f'\n\n[`UPSTREAM.md` at `{FIXTURE_OLD_PIN[:7]}`](https://github.com/owner/tokscale-core/blob/{FIXTURE_OLD_PIN}/UPSTREAM.md)\n')),[])
        def test_engine_blob_link_at_the_pin_passes_unlabelled(self):
            self.assertEqual(validate(self.append_doc(self.root(),f'\n\n[`UPSTREAM.md`](https://github.com/owner/tokscale-core/blob/{FIXTURE_PIN}/UPSTREAM.md)\n')),[])
        def test_link_label_disagreeing_with_its_target_is_reported(self):
            r=self.append_doc(self.root(),f'\n\n[`UPSTREAM.md` at `{FIXTURE_OLD_PIN[:7]}`](https://github.com/owner/tokscale-core/blob/{FIXTURE_PIN}/UPSTREAM.md)\n')
            self.assertIn('label does not match its target','\n'.join(map(str,validate(r))))
        def test_shallow_engine_skips_sha_existence_but_keeps_pin_checks(self):
            import unittest.mock as mock
            r=self.append_doc(self.root(),f'\n\nNative reviewed pin is `{FIXTURE_OLD_PIN}`.\n')
            with mock.patch(f'{__name__}.engine_is_shallow',return_value=True), \
                 mock.patch(f'{__name__}.gitlink_pin',return_value=FIXTURE_PIN):
                s='\n'.join(map(str,validate(r)))
            self.assertIn('stale reviewed pin',s)
            self.assertNotIn('does not exist',s)
        def test_gitlink_follows_an_uncommitted_submodule_checkout(self):
            """A pin advance moves the submodule and edits the documents, then runs
            this before committing. Reading the committed tree would reject every
            correct advance, so the value must follow the submodule's own HEAD."""
            import subprocess
            r=self.root(); engine=r/'vendor'/'tokscale-core'; engine.mkdir(parents=True,exist_ok=True)
            def git(*a,cwd=engine): subprocess.run(['git',*a],cwd=cwd,capture_output=True,check=True)
            git('init','-q'); git('-c','user.email=t@t','-c','user.name=t','commit','-q','--allow-empty','-m','one')
            first=subprocess.run(['git','rev-parse','HEAD'],cwd=engine,capture_output=True,text=True).stdout.strip()
            self.assertEqual(gitlink_pin(r),first)
            git('-c','user.email=t@t','-c','user.name=t','commit','-q','--allow-empty','-m','two')
            second=subprocess.run(['git','rev-parse','HEAD'],cwd=engine,capture_output=True,text=True).stdout.strip()
            self.assertNotEqual(first,second)
            self.assertEqual(gitlink_pin(r),second,'must track the submodule checkout, not a committed gitlink')
        def test_delta_count_disagreeing_with_the_engine_is_reported(self):
            import unittest.mock as mock
            r=self.append_doc(self.root(),'\n\n本次 `aaaaaaa` → `bbbbbbb` delta 為 10 個 engine commit。\n')
            with mock.patch(f'{__name__}.engine_rev_count',return_value=7):
                self.assertIn('delta count disagrees','\n'.join(map(str,validate(r))))
        def test_delta_count_matching_the_engine_passes(self):
            import unittest.mock as mock
            r=self.append_doc(self.root(),'\n\n本次 `aaaaaaa` → `bbbbbbb` delta 為 7 個 engine commit。\n')
            with mock.patch(f'{__name__}.engine_rev_count',return_value=7):
                self.assertEqual(validate(r),[])
        def test_uncountable_delta_does_not_block(self):
            """A shallow clone cannot count a range it does not hold; silence beats a wrong number."""
            import unittest.mock as mock
            r=self.append_doc(self.root(),'\n\n本次 `aaaaaaa` → `bbbbbbb` delta 為 999 個 engine commit。\n')
            with mock.patch(f'{__name__}.engine_rev_count',return_value=None):
                self.assertEqual(validate(r),[])
        def test_delta_count_is_derived_from_a_real_repository(self):
            """Without mocking `engine_rev_count`, so a check that never runs fails here.

            The mocked tests above prove the comparison; they cannot prove it is
            reached. In a shallow checkout it is not, which is how a stale count
            passed CI three advances running."""
            import subprocess
            r=self.root(); engine=r/'vendor'/'tokscale-core'; engine.mkdir(parents=True,exist_ok=True)
            def git(*a): subprocess.run(['git',*a],cwd=engine,capture_output=True,check=True)
            git('init','-q')
            shas=[]
            for i in range(4):
                git('-c','user.email=t@t','-c','user.name=t','commit','-q','--allow-empty','-m',f'c{i}')
                shas.append(subprocess.run(['git','rev-parse','HEAD'],cwd=engine,capture_output=True,text=True).stdout.strip())
            base,head=shas[0],shas[3]
            self.assertEqual(engine_rev_count(r,base,head),3,'fixture must have a countable range')
            self.append_doc(r,f'\n\n本次 `{base[:7]}` → `{head[:7]}` delta 為 99 個 engine commit。\n')
            self.assertIn('delta count disagrees','\n'.join(map(str,validate(r))))
        def test_unresolvable_delta_endpoints_are_reported(self):
            """A typo in an endpoint must not silence the count check.

            `git rev-list` exits non-zero for an unknown revision, which is an
            answer, not an inability to ask -- collapsing the two lets anyone
            disable the check by mistyping a SHA."""
            import subprocess
            r=self.root(); engine=r/'vendor'/'tokscale-core'; engine.mkdir(parents=True,exist_ok=True)
            subprocess.run(['git','init','-q'],cwd=engine,capture_output=True,check=True)
            subprocess.run(['git','-c','user.email=t@t','-c','user.name=t','commit','-q','--allow-empty','-m','c'],cwd=engine,capture_output=True,check=True)
            self.append_doc(r,'\n\n本次 `deadbee` → `f00dfac` delta 為 3 個 engine commit。\n')
            self.assertIn('do not resolve in the engine','\n'.join(map(str,validate(r))))
        def test_missing_reviewed_pin_row_is_reported(self):
            r=self.root(); (r/'vendor/README.md').write_text('[Knowledge](../docs/knowledge/vendor-tokscale.md)')
            self.assertIn('reviewed pin row is missing','\n'.join(map(str,validate(r))))
        def test_gitlink_disagreement_is_reported(self):
            import unittest.mock as mock
            r=self.root()
            with mock.patch(f'{__name__}.gitlink_pin',return_value=FIXTURE_OLD_PIN):
                self.assertIn('does not match the gitlink','\n'.join(map(str,validate(r))))
        def test_gitlink_agreement_passes(self):
            import unittest.mock as mock
            r=self.root()
            with mock.patch(f'{__name__}.gitlink_pin',return_value=FIXTURE_PIN):
                self.assertEqual(validate(r),[])
        def test_absent_gitlink_does_not_block(self):
            import unittest.mock as mock
            r=self.root()
            with mock.patch(f'{__name__}.gitlink_pin',return_value=None):
                self.assertEqual(validate(r),[])
        def test_nonexistent_engine_sha_is_reported(self):
            import unittest.mock as mock
            r=self.append_doc(self.root(),f'\n\n[`UPSTREAM.md` at `{FIXTURE_OLD_PIN[:7]}`](https://github.com/owner/tokscale-core/blob/{FIXTURE_OLD_PIN}/UPSTREAM.md)\n')
            with mock.patch(f'{__name__}.sha_exists_in_engine',side_effect=lambda root,sha: sha!=FIXTURE_OLD_PIN):
                self.assertIn('does not exist','\n'.join(map(str,validate(r))))
        def test_vendor_readme_is_held_to_the_frontmatter_contract(self):
            r=self.root(); v=r/'vendor/README.md'; v.write_text(v.read_text().split('---\n',2)[2])
            self.assertIn('vendor/README.md:1: missing YAML frontmatter','\n'.join(map(str,validate(r))))
        def git_repo(self,r):
            import subprocess
            def git(*a): return subprocess.run(['git','-c','user.email=t@t','-c','user.name=t',*a],cwd=r,capture_output=True,text=True,check=True).stdout.strip()
            git('init','-q'); return git
        def test_current_advance_claim_is_checked_against_the_diff(self):
            import unittest.mock as mock
            r=self.append_doc(self.root(),'\n\n這次 consumer 是 pin-only：`crates/tb_core_ffi` 零改動。\n\n該次 consumer 同樣是 pin-only，`crates/tb_core_ffi` 零改動。\n')
            with mock.patch(f'{__name__}.advance_touched',return_value={'crates/tb_core_ffi'}):
                errs=[str(e) for e in validate(r)]
            self.assertEqual(len(errs),1,errs)
            self.assertIn('left crates/tb_core_ffi unchanged',errs[0])
            with mock.patch(f'{__name__}.advance_touched',return_value=set()):
                self.assertEqual(validate(r),[],'an advance that left the crate alone must not flag the claim')
        def test_advance_touched_reads_a_real_history(self):
            """Unmocked, so a check that never reaches git fails here."""
            r=self.root(); git=self.git_repo(r)
            (r/'crates/tb_core_ffi').mkdir(parents=True); (r/'crates/tb_core_ffi/lib.rs').write_text('a')
            # `git add -A` would drop the gitlink (no directory backs it), so
            # stage by path after the gitlink is in the index.
            git('add','-A'); git('update-index','--add','--cacheinfo',f'160000,{FIXTURE_OLD_PIN},vendor/tokscale-core')
            git('commit','-q','-m','base')
            self.assertEqual(git('rev-parse','HEAD:vendor/tokscale-core'),FIXTURE_OLD_PIN,'fixture must carry a gitlink')
            self.assertIsNone(advance_touched(r),'a root commit has no parent to compare')
            git('update-index','--cacheinfo',f'160000,{FIXTURE_PIN},vendor/tokscale-core'); git('commit','-q','-m','pin-only advance')
            self.assertEqual(advance_touched(r),set())
            (r/'crates/tb_core_ffi/lib.rs').write_text('b'); git('add','crates'); git('commit','-q','-m','later ffi work')
            self.assertEqual(advance_touched(r),set(),'FFI work after a pin-only advance is not part of that advance')
            (r/'crates/tb_core_ffi/lib.rs').write_text('c'); git('add','crates')
            git('update-index','--cacheinfo',f'160000,{FIXTURE_OLD_PIN},vendor/tokscale-core'); git('commit','-q','-m','advance with ffi')
            self.assertEqual(advance_touched(r),{'crates/tb_core_ffi'})
            (r/'README.md').write_text('changed'); git('add','README.md'); git('commit','-q','-m','unrelated')
            self.assertEqual(advance_touched(r),{'crates/tb_core_ffi'},'measured at the advance, not at HEAD')
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
        def test_quoted_and_earlier_clauses_are_not_current_claims(self):
            import unittest.mock as mock
            r=self.append_doc(self.root(),'\n\n下面關於「這次 consumer 是 pin-only、`ctb.h` 簽名不變」的敘述只描述推進。\n\nThis advance moves the pin; that earlier advance left `crates/tb_core_ffi` unchanged.\n\nThis advance leaves `crates/tb_core_ffi` unchanged.\n')
            with mock.patch(f'{__name__}.advance_touched',return_value=set(ADVANCE_PATHS)):
                errs=[str(e) for e in validate(r)]
            self.assertEqual(len(errs),1,errs)
            self.assertIn('left crates/tb_core_ffi unchanged',errs[0])
        def test_advance_on_this_branch_counts_earlier_branch_commits(self):
            """Locally the advance is the branch, not only the commit that moved the gitlink."""
            r=self.root(); git=self.git_repo(r)
            (r/'crates/tb_core_ffi').mkdir(parents=True); (r/'crates/tb_core_ffi/lib.rs').write_text('a')
            git('add','-A'); git('update-index','--add','--cacheinfo',f'160000,{FIXTURE_OLD_PIN},vendor/tokscale-core'); git('commit','-q','-m','base')
            git('update-ref','refs/remotes/origin/main','HEAD')
            (r/'crates/tb_core_ffi/lib.rs').write_text('b'); git('add','crates'); git('commit','-q','-m','ffi first')
            git('update-index','--cacheinfo',f'160000,{FIXTURE_PIN},vendor/tokscale-core'); git('commit','-q','-m','then the advance')
            self.assertEqual(advance_touched(r),{'crates/tb_core_ffi'})
        def test_committed_branch_advance_with_stale_checkout_is_not_missed(self):
            import subprocess
            r=self.root(); git=self.git_repo(r)
            engine=r/'vendor/tokscale-core'; engine.mkdir(parents=True,exist_ok=True)
            subprocess.run(['git','init','-q'],cwd=engine,check=True,capture_output=True)
            subprocess.run(['git','-c','user.email=t@t','-c','user.name=t','commit','-q','--allow-empty','-m','e'],cwd=engine,check=True,capture_output=True)
            old_pin=subprocess.run(['git','rev-parse','HEAD'],cwd=engine,capture_output=True,text=True).stdout.strip()
            (r/'crates/tb_core_ffi').mkdir(parents=True); (r/'crates/tb_core_ffi/lib.rs').write_text('a')
            git('add','crates','docs','README.md','AGENTS.md','CLAUDE.md','landing','vendor/README.md','vendor/AGENTS.md')
            git('update-index','--add','--cacheinfo',f'160000,{old_pin},vendor/tokscale-core'); git('commit','-q','-m','base')
            git('update-ref','refs/remotes/origin/main','HEAD')
            (r/'crates/tb_core_ffi/lib.rs').write_text('b'); git('add','crates')
            git('update-index','--cacheinfo',f'160000,{FIXTURE_PIN},vendor/tokscale-core'); git('commit','-q','-m','advance, checkout left behind')
            self.assertEqual(gitlink_pin(r),old_pin,'fixture: checkout still at the old pin')
            self.assertEqual(advance_touched(r),{'crates/tb_core_ffi'})
        def test_uncommitted_advance_is_measured_against_head(self):
            import subprocess
            r=self.root(); git=self.git_repo(r)
            (r/'crates/tb_core_ffi').mkdir(parents=True); (r/'crates/tb_core_ffi/lib.rs').write_text('a')
            git('add','-A'); git('update-index','--add','--cacheinfo',f'160000,{FIXTURE_OLD_PIN},vendor/tokscale-core'); git('commit','-q','-m','base')
            engine=r/'vendor/tokscale-core'; engine.mkdir(parents=True,exist_ok=True)
            subprocess.run(['git','init','-q'],cwd=engine,check=True,capture_output=True)
            subprocess.run(['git','-c','user.email=t@t','-c','user.name=t','commit','-q','--allow-empty','-m','e'],cwd=engine,check=True,capture_output=True)
            self.assertEqual(advance_touched(r),set(),'checkout moved, FFI untouched')
            (r/'crates/tb_core_ffi/lib.rs').write_text('b')
            self.assertEqual(advance_touched(r),{'crates/tb_core_ffi'},'uncommitted FFI edit during an advance')
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
        def test_external_engine_docs_are_ignored(self):
            r=self.root(); engine=r/'vendor/tokscale-core'; engine.mkdir()
            (engine/'AGENTS.md').write_text('secret = sk-live-1\n[missing](nope.md)')
            self.assertEqual(validate(r),[])
        def test_skip_named_parent_does_not_hide_repo(self):
            result='\n'.join(map(str,validate(self.root(True,'target')))); self.assertIn('invalid scope',result); self.assertNotIn('knowledge tree is missing',result)
        def test_root_adapter_contract(self):
            r=self.root(); (r/'CLAUDE.md').write_text('[missing](nope.md) /Users/local/file')
            s='\n'.join(map(str,validate(r))); self.assertIn('missing adapter link target',s); self.assertIn('machine-local path',s); self.assertIn('root CLAUDE.md must route',s)
        def test_contributing_adapter_contract(self):
            r=self.root(); (r/'CONTRIBUTING.md').write_text('[Knowledge](docs/knowledge/README.md) [missing](nope.md) /Users/local/file')
            s='\n'.join(map(str,validate(r))); self.assertIn('CONTRIBUTING.md:1: missing adapter link target',s); self.assertIn('CONTRIBUTING.md:1: machine-local path',s)
        def test_linux_home_path(self):
            r=self.root(); (r/'CLAUDE.md').write_text('See AGENTS.md /home/alice/workspace/file')
            self.assertIn('machine-local path','\n'.join(map(str,validate(r))))
        def test_absolute_workspace_paths(self):
            for path in ('/workspace/alice/private-repo','/workspaces/team/private-repo'):
                r=self.root(); (r/'CLAUDE.md').write_text(f'See AGENTS.md {path}')
                self.assertIn('machine-local path','\n'.join(map(str,validate(r))))
        def test_windows_user_path(self):
            r=self.root(); (r/'CLAUDE.md').write_text('See AGENTS.md C:\\Users\\alice\\workspace\\private-repo')
            self.assertIn('machine-local path','\n'.join(map(str,validate(r))))
        def test_private_ledger_row(self):
            r=self.root(); p=r/'docs/knowledge/ledger.md'; text=p.read_text().replace('public | migrated','project-private | migrated',1).replace('topic-001','token credentials'); p.write_text(text); self.assertIn('private ledger row', '\n'.join(map(str,validate(r))))
        def test_invalid_date(self):
            r=self.root(); p=r/'docs/knowledge/ledger.md'; p.write_text(p.read_text().replace('last_verified: 2026-07-14','last_verified: 2026-7-14')); result='\n'.join(map(str,validate(r))); self.assertIn('last_verified must be a valid YYYY-MM-DD date',result)
        def test_missing_path_source(self):
            r=self.root(); p=r/'docs/knowledge/ledger.md'; p.write_text(p.read_text().replace('sources: [internal]','sources: [docs/knowledge/missing.md]')); result='\n'.join(map(str,validate(r))); self.assertIn('source path does not exist',result)
        def test_uninitialized_declared_submodule_source(self):
            for empty_root in (False, True):
                with self.subTest(empty_root=empty_root):
                    r=self.root(); (r/'.gitmodules').write_text('[submodule "vendor/tokscale-core"]\n\tpath = vendor/tokscale-core\n')
                    if empty_root: (r/'vendor/tokscale-core').mkdir()
                    p=r/'docs/knowledge/ledger.md'; p.write_text(p.read_text().replace('sources: [internal]','sources: [vendor/tokscale-core/src/missing.rs]'))
                    self.assertEqual(validate(r),[])
        def test_populated_submodule_missing_source(self):
            r=self.root(); (r/'.gitmodules').write_text('[submodule "vendor/tokscale-core"]\n\tpath = vendor/tokscale-core\n')
            engine=r/'vendor/tokscale-core'; engine.mkdir(); (engine/'.git').write_text('gitdir: elsewhere')
            p=r/'docs/knowledge/ledger.md'; p.write_text(p.read_text().replace('sources: [internal]','sources: [vendor/tokscale-core/src/missing.rs]'))
            self.assertIn('source path does not exist','\n'.join(map(str,validate(r))))
        def test_undeclared_submodule_missing_source(self):
            r=self.root(); (r/'vendor/tokscale-core').mkdir()
            p=r/'docs/knowledge/ledger.md'; p.write_text(p.read_text().replace('sources: [internal]','sources: [vendor/tokscale-core/src/missing.rs]'))
            self.assertIn('source path does not exist','\n'.join(map(str,validate(r))))
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
