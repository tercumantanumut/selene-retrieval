"""Average MTEB(eng v1) Retrieval nDCG@10 over the 15 BEIR tasks, read live from HF model-card model-index."""
import json,sys,urllib.request
BEIR=['ArguAna','ClimateFEVER','CQADupstack','DBPedia','FEVER','FiQA2018','HotpotQA','MSMARCO','NFCorpus','NQ','QuoraRetrieval','SCIDOCS','SciFact','Touche2020','TRECCOVID']
def norm(n): return n.replace('MTEB ','').split(' (')[0].strip()
for m in sys.argv[1:]:
    try: d=json.load(urllib.request.urlopen(f'https://huggingface.co/api/models/{m}'))
    except Exception as e: print(m,'ERR',e); continue
    mi=(d.get('cardData') or {}).get('model-index') or []
    scores={}; cqa=[]
    for r in (mi[0]['results'] if mi else []):
        if r.get('task',{}).get('type')!='Retrieval': continue
        name=norm(r['dataset'].get('name',''))
        nd=[x['value'] for x in r['metrics'] if x['type']=='ndcg_at_10']
        if not nd: continue
        v=nd[0]; v=v*100 if v<=1 else v
        if name.startswith('CQADupstack'): cqa.append(v)
        elif name in BEIR: scores[name]=v
    if cqa: scores['CQADupstack']=sum(cqa)/len(cqa)
    miss=[b for b in BEIR if b not in scores]
    avg=sum(scores.values())/len(scores) if scores else None
    print(f'{m}: n={len(scores)} avg_ndcg10={avg and round(avg,2)} missing={miss}')
