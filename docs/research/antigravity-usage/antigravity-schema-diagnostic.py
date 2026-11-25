#!/usr/bin/env python3
"""Read-only, bounded metadata diagnostic for tapes' T3-managed Antigravity ACP.
No raw records, paths, model names, session IDs, response IDs, token values,
SQL DDL/defaults, protobuf values, or exception messages are emitted.
SQLite requires an explicitly quiescent store (or an offline consistent snapshot).
Only schema identifiers are printed; inspect the report before sharing it.
"""
import argparse
from collections import Counter, defaultdict
import json
import os
from pathlib import Path
import re
import sqlite3
import stat
import time
import uuid

COUNTERS = ('input_tokens','cache_read_tokens','output_tokens','thinking_tokens',
            'reasoning_tokens','cache_write_tokens','total_tokens', 'prompt_tokens',
            'cached_input_tokens','uncached_input_tokens','completion_tokens')
IDS = ('response_id','responseId','request_id','requestId','message_id','messageId',
       'provider_response_id','providerResponseId','execution_id','executionId')
KEYS = frozenset(COUNTERS + IDS + (
    'inputTokens','outputTokens','cacheReadTokens','cacheWriteTokens','thinkingTokens',
    'reasoningTokens','totalTokens','inputTokenCount','outputTokenCount',
    'promptTokenCount','candidatesTokenCount','cachedContentTokenCount','thoughtsTokenCount',
    'totalTokenCount','input','output','reasoning','cache_read','cache_write','cost','usd',
    'cost_usd','costUsd','total_cost','totalCost','price','price_usd','credits','billing',
    'usage','usage_metadata','usageMetadata','token_usage','tokenUsage','tokens',
    'metadata','response','request','result','model','model_id','modelId','modelName',
    'step_index','stepIndex','step_type','status','type','source','created_at',
    'context_window','contextWindow','retry','retries','attempt','attempts',
    'generation','generation_metadata','generationMetadata','total_usage','totalUsage',
    'parent','parentId','conversationId','cascade_id','spawnStepIndex','provider','data',
    'cache_creation_input_tokens','cache_read_input_tokens','input_tokens_details',
    'output_tokens_details','cached_tokens','reasoning_tokens','usageComplete','usage_complete',
    'usageDelta','usage_delta','usageTotal','usage_total','total_token_usage','last_token_usage'))
CONTAINERS = frozenset(('usage','usage_metadata','usageMetadata','token_usage','tokenUsage',
                        'tokens','metadata','response','request','result','billing','retry','retries',
                        'attempt','attempts','generation','generation_metadata','generationMetadata',
                        'total_usage','totalUsage','data','input_tokens_details','output_tokens_details',
                        'usageDelta','usage_delta','usageTotal','usage_total','total_token_usage','last_token_usage'))
TYPES = frozenset(('USER_INPUT','PLANNER_RESPONSE','GENERIC','SYSTEM_MESSAGE','CHECKPOINT',
                   'CONVERSATION_HISTORY','FINISH','ERROR_MESSAGE','EPHEMERAL','COMMAND_STATUS',
                   'RUN_COMMAND','VIEW_FILE','LIST_DIRECTORY','CODE_ACTION','MCP_TOOL'))
STATUSES = frozenset(('DONE','ERROR','CANCELED','CANCELLED','INTERRUPTED','PENDING','RUNNING','WAITING'))
SOURCES = frozenset(('MODEL','USER_EXPLICIT','USER_IMPLICIT','SYSTEM'))
T3_TABLES = ('orchestration_v2_projection_threads','orchestration_v2_projection_provider_threads',
             'orchestration_v2_projection_provider_turns','orchestration_v2_projection_turn_items',
             'orchestration_v2_projection_runs','orchestration_v2_projection_run_attempts',
             'orchestration_v2_projection_subagents')
# Only a fixed vocabulary of JSON keys and safe type categories can reach stdout.
# Unrecognized keys are counted, never printed, even if their name suggests usage.

def regular(p):
    try: return stat.S_ISREG(p.lstat().st_mode)
    except OSError: return False

def directory(p):
    try: return stat.S_ISDIR(p.lstat().st_mode)
    except OSError: return False

def version4(s):
    try: return str(uuid.UUID(s)) == s.lower() and uuid.UUID(s).version == 4
    except (ValueError, AttributeError): return False

def safe_ident(s):
    return s if re.fullmatch(r'[A-Za-z_][A-Za-z_0-9]{0,127}', s or '') else '<nonstandard-identifier>'

def quote(s):
    return '"' + s.replace('"','""') + '"'

def num(v):
    return type(v) is int and 0 <= v <= 2**64 - 1

def kind(v):
    if v is None: return 'null'
    if type(v) is bool: return 'boolean'
    if num(v): return 'u64'
    if isinstance(v, int): return 'other-integer'
    if isinstance(v, float): return 'float'
    if isinstance(v, str): return 'string'
    if isinstance(v, dict): return 'object'
    if isinstance(v, list): return 'array'
    return 'other'

def group(v, allowed):
    return v if isinstance(v, str) and v in allowed else 'other-or-missing'

def shape(obj, tally, prefix='', depth=0):
    if depth > 5 or not isinstance(obj, dict): return
    for k, v in obj.items():
        if k not in KEYS:
            # Never print an arbitrary dynamic JSON key (it may be user text).
            if re.search(r'token|usage|cost|bill|price|credit|cache|response.?id|request.?id', k, re.I):
                tally[prefix+'<unlisted-usage-like-key>:'+kind(v)] += 1
            continue
        path = prefix+k
        tally[path+':'+kind(v)] += 1
        if k in CONTAINERS:
            if isinstance(v, dict): shape(v,tally,path+'.',depth+1)
            elif isinstance(v,list):
                for item in v[:64]: shape(item,tally,path+'[].',depth+1)
                if len(v)>64: tally[path+':array-items-omitted'] += len(v)-64

def no_duplicate_pairs(pairs):
    out = {}
    for k,v in pairs:
        if k in out: raise ValueError('duplicate key')
        out[k]=v
    return out

def decode_json(raw):
    return json.loads(raw,object_pairs_hook=no_duplicate_pairs,
                      parse_constant=lambda _: (_ for _ in ()).throw(ValueError('nonfinite')))

def selected_usage(rec):
    return tuple(rec.get(k) if num(rec.get(k)) else None for k in COUNTERS)

def jsonl(path,args):
    out = {'status':'missing-or-not-regular'}
    # Require the entire expected brain/log hierarchy to consist of native directories.
    if not regular(path) or any(not directory(p) for p in list(path.parents)[:5]):
        return out,{},set()
    tallies=defaultdict(Counter); events=Counter(); rel=Counter(); by_step={}; duplicate=set()
    ids=set(); local_ids=set(); prior={}; signature_by_step={}; records=0; bytes_read=0
    limit=args.max_mib*1024*1024; max_line=min(limit,4*1024*1024)
    deadline=time.monotonic()+args.seconds
    try:
        before=path.stat()
        with path.open('rb') as f:
            while records<args.max_rows and bytes_read<limit and time.monotonic()<deadline:
                # Do not read appended data beyond the initial size.
                remain=min(limit-bytes_read,before.st_size-bytes_read)
                if remain<=0: break
                line=f.readline(min(max_line+1,remain))
                if not line: break
                bytes_read+=len(line); records+=1
                if not line.endswith(b'\n') and bytes_read<before.st_size:
                    events['oversized-or-boundary-line']+=1; break
                if len(line)>max_line:
                    events['oversized-or-boundary-line']+=1; break
                if not line.endswith(b'\n'):
                    events['unterminated-final-line']+=1; break
                try: rec=decode_json(line)
                except (ValueError,UnicodeError,RecursionError): events['malformed-json']+=1; continue
                if not isinstance(rec,dict): events['non-object']+=1; continue
                typ=group(rec.get('type'),TYPES); status=group(rec.get('status'),STATUSES)
                src=group(rec.get('source'),SOURCES); tag=f'{typ}/{src}/{status}'
                events[tag]+=1; shape(rec,tallies[tag]); usage=selected_usage(rec)
                has=any(x is not None for x in usage)
                if typ=='PLANNER_RESPONSE' and src=='MODEL':
                    rel['planner-model-rows']+=1
                    if not has: rel['planner-without-known-counters']+=1
                if not has: continue
                rel['rows-with-known-counters']+=1
                idx=rec.get('step_index'); model=rec.get('model')
                model=model if isinstance(model,str) and len(model)<=256 else None
                if num(idx):
                    sig=(usage,model)
                    if idx in signature_by_step:
                        duplicate.add(idx); rel['repeated-usage-step-index']+=1
                        rel['repeated-step-same-counters-and-model' if sig==signature_by_step[idx]
                            else 'repeated-step-conflicting-counters-or-model']+=1
                    else: signature_by_step[idx]=sig; by_step[idx]=usage
                else: rel['usage-row-without-valid-step-index']+=1
                for key in IDS:
                    v=rec.get(key)
                    if isinstance(v,str) and 0<len(v)<=1024:
                        pair=(key,v)
                        if pair in local_ids: rel['repeated-top-level-id:'+key]+=1
                        local_ids.add(pair); ids.add(pair)
                i,c,o,t,r,w,total,*_=usage
                if i is not None and c is not None:
                    rel['cache-read-greater-than-input' if c>i else 'cache-read-not-greater-than-input']+=1
                if o is not None:
                    for name,v in [('thinking',t),('reasoning',r)]:
                        if v is not None: rel[name+'-le-output' if v<=o else name+'-gt-output']+=1
                if i is not None and c is not None and o is not None and total is not None:
                    rel['total-equals-input-plus-cache-plus-output'+('-yes' if total==i+c+o else '-no')]+=1
                    if w is not None: rel['total-equals-input-plus-cache-plus-write-plus-output'+('-yes' if total==i+c+w+o else '-no')]+=1
                if model is not None and model in prior:
                    pairs=[(a,b) for a,b in zip(prior[model],usage) if a is not None and b is not None]
                    if pairs:
                        rel['adjacent-same-model-common-counter-decreased' if any(b<a for a,b in pairs)
                            else 'adjacent-same-model-common-counters-nondecreasing']+=1
                if model is not None: prior[model]=usage
        after=path.stat(); stable=(before.st_ino,before.st_size,before.st_mtime_ns)==(after.st_ino,after.st_size,after.st_mtime_ns)
        out={'status':'read','records_examined':records,'reached_initial_eof':bytes_read==before.st_size,
             'stable_stat':stable,'record_counts':dict(events),'counter_relationships':dict(rel),
             'known_key_type_counts':{k:dict(v) for k,v in tallies.items()}}
        if bytes_read<before.st_size: out['bound_or_unreadable_line']=True
        for idx in duplicate: by_step.pop(idx,None) # never choose between duplicate witnesses
        return out,by_step,ids
    except OSError: return {'status':'read-error'}, {}, set()

# A deliberately minimal wire reader. It never returns data to stdout: only
# shape histograms and selected numeric equality comparisons are reported.
def varint(b,p):
    v=0
    for shift in range(0,70,7):
        if p>=len(b): raise ValueError('short')
        x=b[p]; p+=1
        if shift==63 and x>1: raise ValueError('overflow')
        v|=(x&127)<<shift
        if x<128: return v,p
    raise ValueError('overflow')

def wire(b):
    p=0; result=defaultdict(list); count=0
    while p<len(b):
        count+=1
        if count>10000: raise ValueError('field bound')
        tag,p=varint(b,p); n=tag>>3; w=tag&7
        if not 0<n<2**29: raise ValueError('field')
        if w==0: v,p=varint(b,p)
        elif w in (1,2,5):
            if w==2: size,p=varint(b,p)
            else: size=8 if w==1 else 4
            if size>len(b)-p: raise ValueError('short')
            v=b[p:p+size]; p+=size
        else: raise ValueError('wire type')
        result[n].append((w,v))
    return result

def singular(msg,n,w):
    a=msg.get(n,[])
    return a[0][1] if len(a)==1 and a[0][0]==w else None

def probe(b,paths,hist,base):
    if not isinstance(b,bytes): return {}
    result={}
    try: root=wire(b)
    except ValueError: hist[base+':wire-invalid']+=1; return result
    for path in paths:
        obj=root; label=base+''.join('.'+str(n) for n in path)
        try:
            for n in path:
                v=singular(obj,n,2)
                if v is None: obj=None; break
                obj=wire(v)
        except ValueError: hist[label+':wire-invalid']+=1; continue
        if obj is None: hist[label+':absent-or-nonsingular']+=1; continue
        hist[label+':messages']+=1
        for n,vals in obj.items():
            for w in set(x[0] for x in vals): hist[f'{label}:field-{n}/wire-{w}']+=1
        result[path]=obj
    return result

def generation_step_indices(root):
    # Candidate CortexStepGeneratorMetadata.step_indices (field 2):
    # repeated uint32/uint64 varints, possibly packed. Table indices are
    # distinct namespaces and must never be used as a cross-table join.
    if root is None: return None
    indices=set(); count=0
    try:
        for w, raw in root.get(2, []):
            if w == 0:
                count+=1; indices.add(raw)
            elif w == 2:
                p=0
                while p<len(raw):
                    count+=1
                    if count>10000: return None
                    idx,p=varint(raw,p); indices.add(idx)
            else: return None
            if count>10000: return None
    except ValueError: return None
    return indices

def compare_usage(msg,values,stats,label):
    if msg is None or values is None: return
    stats[label+':matched-unique-jsonl-step']+=1
    # These are hypotheses from external research, not schema declarations.
    for field,ix,name in [(2,0,'input'),(3,2,'output'),(4,5,'cache_write'),(5,1,'cache_read'),(9,3,'thinking'),(9,4,'reasoning')]:
        a=singular(msg,field,0); b=values[ix]
        if a is not None and b is not None:
            stats[f'{label}:{name}:'+('equal' if a==b else 'different')]+=1
    out=singular(msg,3,0); a=singular(msg,9,0); b=singular(msg,10,0)
    if out is not None and a is not None and b is not None:
        stats[label+':field9-plus-field10-equals-field3:'+('yes' if a+b==out else 'no')]+=1

def candidate_retries(parent, field, hist, label):
    # Candidate repeated RetryInfo messages, each containing a usage message
    # at field 2. Neither count nor amount is promoted to actual consumption.
    if parent is None: return
    for w, raw in parent.get(field, [])[:128]:
        if w != 2: hist[label+':wrong-wire']+=1; continue
        decoded=probe(raw,[(),(2,)],hist,label)
        if (2,) in decoded: yield decoded[(2,)]
    if len(parent.get(field, []))>128: hist[label+':retry-bound']+=1

def observe_candidate(msg,source,seen,stats):
    if msg is None: return
    stats[source+':usage-messages']+=1
    # External decoder's candidate identity fields. Equal bytes are witnesses,
    # not proof of an account namespace or that conflicting amounts are valid.
    amounts=tuple(singular(msg,n,0) for n in (2,3,4,5,9,10))
    for field,label in ((7,'message'),(11,'response'),(12,'provider-assigned-message')):
        raw=singular(msg,field,2)
        if not raw or len(raw)>1024: continue
        stats[source+':has-'+label+'-id']+=1
        prior=seen.setdefault((field,raw),{})
        for other,old in prior.items():
            if other==source:
                stats[source+':repeated-'+label+'-id']+=1
            else:
                pair=' / '.join(sorted((source,other)))
                stats[pair+':shared-'+label+'-id:'+('same-counters' if old==amounts else 'different-or-partial-counters')]+=1
        prior.setdefault(source,amounts)

def sig(path):
    st=path.stat(); return st.st_ino,st.st_size,st.st_mtime_ns

def sidecars(path):
    # lexists catches dangling symlinks as well. Never open these files.
    return [x for x in ('-wal','-shm','-journal') if os.path.lexists(str(path)+x)]

def sqlite_report(path,args,by_step=None,expected_id=None,t3=False):
    if not regular(path): return {'status':'missing-or-not-regular'}
    side=sidecars(path)
    if not args.sqlite_quiescent: return {'status':'not-opened-requires-quiescent-flag','sidecars_present':side}
    if side: return {'status':'not-opened-sidecar-present','sidecars_present':side}
    out={'status':'read','tables':{}}; conn=None; start=time.monotonic(); by_step=by_step or {}
    try:
        before=sig(path)
        # immutable forbids SQLite sidecar creation, but is only appropriate for
        # a closed store with no WAL/journal, or a consistent offline snapshot.
        conn=sqlite3.connect(path.absolute().as_uri()+'?mode=ro&immutable=1',uri=True,timeout=0)
        conn.execute('PRAGMA query_only=ON'); conn.execute('PRAGMA trusted_schema=OFF')
        conn.execute('PRAGMA temp_store=MEMORY')
        conn.set_progress_handler(lambda: int(time.monotonic()-start>args.seconds),1000)
        tables={}
        for (name,) in conn.execute("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' LIMIT 129"):
            if len(tables)>=128: out['table_bound']=True; break
            # Only report name + declared affinity, no DDL, defaults or values.
            # Do not query arbitrary virtual/shadow tables for content.
            cols={}
            for row in conn.execute('PRAGMA table_info('+quote(name)+')'):
                col,decl=row[1],row[2].upper(); affinity=('INTEGER' if 'INT' in decl else 'TEXT' if any(x in decl for x in ('CHAR','CLOB','TEXT')) else 'BLOB' if not decl or 'BLOB' in decl else 'REAL' if any(x in decl for x in ('REAL','FLOA','DOUB')) else 'NUMERIC')
                cols[col]={'affinity':affinity,'pk':bool(row[5])}
            tables[name]=cols
            out['tables'][safe_ident(name)]={safe_ident(k):v for k,v in cols.items()}
        out['user_version']=conn.execute('PRAGMA user_version').fetchone()[0]
        if not t3 and 'cascade_id' in tables.get('trajectory_meta',{}):
            rows=conn.execute('SELECT DISTINCT cascade_id FROM trajectory_meta LIMIT 2').fetchall()
            out['identity_matches_filename']=len(rows)==1 and rows[0][0]==expected_id
        if t3:
            out['projection_shapes']={}
            for name in T3_TABLES:
                if 'payload_json' not in tables.get(name,{}): continue
                tally=Counter(); rows=Counter()
                for (length,val) in conn.execute(f'SELECT length(payload_json), CASE WHEN typeof(payload_json)=\'text\' AND length(CAST(payload_json AS BLOB))<=? THEN payload_json END FROM {quote(name)} LIMIT ?', (args.max_blob_kib*1024,args.db_rows+1)):
                    if rows['sampled']>=args.db_rows: rows['row_bound']+=1; break
                    rows['sampled']+=1
                    if not isinstance(val,str): rows['missing-or-oversized-or-nontext']+=1; continue
                    try: shape(decode_json(val),tally)
                    except (ValueError,UnicodeError,RecursionError): rows['malformed-json']+=1
                out['projection_shapes'][name]={'sample':dict(rows),'known_key_type_counts':dict(tally)}
        elif out.get('identity_matches_filename') is not False:
            out['blob_samples']={}
            if 'parent_references' in tables:
                out['parent_reference_rows_present']=bool(conn.execute('SELECT 1 FROM parent_references LIMIT 1').fetchone())
            witness_ids={}; witness_stats=Counter()
            for name, candidates in [('steps',('metadata','step_payload')),('gen_metadata',('data',))]:
                cols=tables.get(name,{}); blobs=[c for c in candidates if c in cols]
                if not blobs: continue
                index='idx' if 'idx' in cols else None
                step_type='step_type' if 'step_type' in cols else None
                step_format='step_format' if 'step_format' in cols else None
                # Only fixed column names; no full record or transcript dump.
                fields=[index or 'NULL',step_type or 'NULL',step_format or 'NULL']
                cap=args.max_blob_kib*1024
                for c in blobs: fields.extend([f'length({c})',f"CASE WHEN typeof({c})='blob' AND length({c})<={cap} THEN {c} END"])
                query=f'SELECT {", ".join(fields)} FROM {name}'+(' ORDER BY idx' if index else '')+' LIMIT ?'
                stats=Counter(); hist=Counter(); meta_eq=Counter(); comparisons=Counter(); processed=0
                for row in conn.execute(query,(args.db_rows+1,)):
                    if processed>=args.db_rows: stats['row_bound']+=1; break
                    processed+=1
                    if time.monotonic()-start>args.seconds: stats['time_bound']+=1; break
                    idx,typ,fmt=row[:3]; typ=typ if num(typ) and typ<=10000 else 'other'; fmt=fmt if num(fmt) and fmt<=10000 else 'other'
                    stats[f'step-type-{typ}/format-{fmt}']+=1
                    seen={}
                    for j,c in enumerate(blobs):
                        length,val=row[3+2*j:5+2*j]; base=name+'.'+c
                        if not isinstance(val,bytes):
                            stats[base+':absent-oversized-or-nonblob']+=1; continue
                        stats[base+':sampled']+=1
                        if name=='gen_metadata': paths=[(),(1,),(1,4),(1,9),(1,9,10),(9,)]
                        elif c=='metadata': paths=[(),(9,)]
                        else: paths=[(),(5,),(5,9)]
                        decoded=probe(val,paths,hist,base); seen[c]=decoded
                        if name=='gen_metadata':
                            msg=decoded.get((1,4)); parent=decoded.get((1,)); retry_field=17
                            indices=generation_step_indices(decoded.get(()))
                            if not indices:
                                comparisons[base+':missing-or-invalid-step-references']+=1
                            else:
                                comparisons[base+':generations-with-step-references']+=1
                                for step_idx in sorted(indices):
                                    compare_usage(msg,by_step.get(step_idx),comparisons,base+':candidate-explicit-step-reference')
                        elif c=='metadata':
                            msg=decoded.get((9,)); parent=decoded.get(()); retry_field=28
                        else:
                            msg=decoded.get((5,9)); parent=decoded.get((5,)); retry_field=28
                        if name=='steps' and typ==15:
                            compare_usage(msg,by_step.get(idx),comparisons,base)
                        observe_candidate(msg,base,witness_ids,witness_stats)
                        for retry in candidate_retries(parent,retry_field,hist,base+':candidate-retries'):
                            observe_candidate(retry,base+':retry',witness_ids,witness_stats)
                    if 'metadata' in seen and 'step_payload' in seen:
                        a=seen['metadata'].get(()); b=seen['step_payload'].get((5,))
                        if a is not None and b is not None: meta_eq['metadata-equals-payload-field5' if a==b else 'metadata-differs-from-payload-field5']+=1
                out['blob_samples'][name]={'rows_examined':processed,'sample_counts':dict(stats),
                    'wire_shape_counts':dict(hist),'mirror_equality':dict(meta_eq),
                    'candidate_jsonl_comparisons':dict(comparisons)}
            out['candidate_usage_identity_witnesses']=dict(witness_stats)
        if before!=sig(path) or sidecars(path): return {'status':'discarded-store-changed-or-sidecar-appeared'}
        return out
    except (sqlite3.Error,OSError,ValueError,TypeError,RecursionError):
        return {'status':'sqlite-error-or-bound','partial_report':out}
    finally:
        if conn is not None: conn.close()

def main():
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--root',type=Path,default=Path(os.environ.get('T3CODE_HOME') or Path.home()/'.t3'),help='T3CODE_HOME (not profile directory)')
    ap.add_argument('--session',type=str,help='Optional exact cascade UUID; never printed')
    ap.add_argument('--sqlite-quiescent',action='store_true',help='Inspect SQLite only after T3/ACP is stopped or on a consistent offline snapshot; skip all DBs with sidecars')
    ap.add_argument('--max-sessions',type=int,default=12)
    ap.add_argument('--max-rows',type=int,default=20000,help='JSONL records per session')
    ap.add_argument('--max-mib',type=int,default=32,help='JSONL bytes per session in MiB')
    ap.add_argument('--db-rows',type=int,default=256,help='sampled rows per SQLite table')
    ap.add_argument('--max-blob-kib',type=int,default=256,help='max examined blob or projection JSON field in KiB')
    ap.add_argument('--seconds',type=int,default=5,help='soft elapsed-time bound per file')
    args=ap.parse_args()
    limits=((args.max_sessions,1,100),(args.max_rows,1,100000),(args.max_mib,1,128),(args.db_rows,1,5000),(args.max_blob_kib,1,4096),(args.seconds,1,60))
    if any(not lo<=val<=hi for val,lo,hi in limits) or (args.session and not version4(args.session)):
        ap.error('invalid limit or non-UUIDv4 session')
    root=args.root.expanduser(); profiles=root/'userdata/providers/antigravity'
    report={'schema':'tapes-antigravity-diagnostic/1','sqlite_quiescent_asserted':args.sqlite_quiescent,
            'bounds':{k:getattr(args,k) for k in ('max_sessions','max_rows','max_mib','db_rows','max_blob_kib','seconds')},
            'sessions':[],'discovery':{},'cross_recording_top_level_id_overlap':{},'t3':{'status':'not-read'}}
    discovery=Counter(); matches=[]; all_ids=Counter(); entries=0; MAX_ENTRIES=10000
    try:
        if not all(directory(p) for p in (root,root/'userdata',root/'userdata/providers',profiles)):
            discovery['store-missing-or-nonnative']+=1
        else:
            for p in profiles.iterdir():
                entries+=1
                if entries>MAX_ENTRIES: discovery['entry-bound']+=1; break
                if not re.fullmatch(r'[0-9a-fA-F]{64}',p.name) or not directory(p): continue
                acp=p/'antigravity-acp'; conv=acp/'conversations'
                if not directory(acp) or not directory(conv): continue
                for db in conv.iterdir():
                    entries+=1
                    if entries>MAX_ENTRIES: discovery['entry-bound']+=1; break
                    if db.suffix!='.db' or not version4(db.stem) or not regular(db): continue
                    if args.session and db.stem.lower()!=args.session.lower(): continue
                    all_ids[db.stem.lower()]+=1; matches.append((acp,db))
                if entries>MAX_ENTRIES: break
    except OSError: discovery['enumeration-error']+=1
    discovery['matching_databases']=len(matches)
    discovery['duplicate_cascade_ids']=sum(v>1 for v in all_ids.values())
    # Newest candidates first; no native IDs, timestamps or paths in the report.
    def order(item):
        try: return item[1].stat().st_mtime_ns
        except OSError: return 0
    matches.sort(key=order,reverse=True)
    id_counts=Counter(); seen_ids=set()
    for acp,db in matches:
        if len(report['sessions'])>=args.max_sessions: discovery['session-bound']+=1; break
        if all_ids[db.stem.lower()]>1: discovery['ambiguous-database-skipped']+=1; continue
        log=acp/'brain'/db.stem/'.system_generated/logs/transcript_full.jsonl'
        j,by_step,ids=jsonl(log,args)
        # IDs are held only in memory for equality, never emitted or hashed to stdout.
        for k,v in ids:
            if (k,v) in seen_ids: id_counts[k]+=1
        seen_ids.update(ids)
        s=sqlite_report(db,args,by_step,db.stem)
        report['sessions'].append({'sample':len(report['sessions'])+1,'canonical_jsonl':j,'conversation_sqlite':s,
                                   'mirror_exists':regular(log.with_name('transcript.jsonl'))})
    report['discovery']=dict(discovery); report['cross_recording_top_level_id_overlap']=dict(id_counts)
    if directory(root) and directory(root/'userdata'):
        report['t3']=sqlite_report(root/'userdata/statev2.sqlite',args,t3=True)
    print(json.dumps(report,indent=2,sort_keys=True))

if __name__=='__main__': main()
