'use client';
import React, {useEffect, useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
type Destination={tenant_id:string;client_id:string;drive_id:string;folder_id:string};
type Status={settings?:{enabled:boolean;destination:Destination;profile?:{displayName:string}};entries:{capture_id:string;status:string;error?:string}[];signing_in:boolean;notice:string};
const blank:Destination={tenant_id:'',client_id:'',drive_id:'',folder_id:''};
export function BtbiUploadSettings(){
 const [status,setStatus]=useState<Status>();const [dest,setDest]=useState<Destination>(blank);
 const [error,setError]=useState('');const [busy,setBusy]=useState(false);
 const [code,setCode]=useState<{user_code:string;verification_uri:string}>();
 useEffect(()=>{let active=true;let first=true;async function refresh(){try{const s=await invoke<Status>('btbi_status');if(!active)return;setStatus(s);if(first&&s.settings){setDest(s.settings.destination);}first=false;}catch(e){if(active)setError(String(e));}}void refresh();const timer=setInterval(refresh,3000);return()=>{active=false;clearInterval(timer);};},[]);
 async function run(action:()=>Promise<unknown>){setBusy(true);setError('');try{await action();}catch(e){setError(String(e));}finally{setBusy(false);}}
 return <section className="space-y-5 p-6 bg-white rounded-lg border">
  <h2 className="text-xl font-semibold">BTBI SharePoint uploads</h2>
  <p>Completed meetings upload audio, transcripts and metadata automatically. Originals remain on this computer. Enable “Save recordings” in Recordings before using automatic upload.</p>
  <details><summary className="cursor-pointer font-medium">One-time administrator setup</summary>
   <p className="my-3 text-sm">Use the BTBI Microsoft app registration and destination folder identifiers supplied by your administrator. These are identifiers, not passwords.</p>
   {([['tenant_id','Tenant ID'],['client_id','Application ID'],['drive_id','Document library ID'],['folder_id','Destination folder ID']] as const).map(([key,label])=><label className="block my-3" key={key}>{label}<input className="block border rounded p-2 w-full" value={dest[key]} onChange={e=>setDest({...dest,[key]:e.target.value})} autoComplete="off" /></label>)}
   <button className="border rounded px-4 py-2" disabled={busy||status?.signing_in} onClick={()=>run(()=>invoke('btbi_configure',{destination:dest,enabled:false}))}>Save setup</button>
  </details>
  <p>{status?.settings?.profile?`Signed in as ${status.settings.profile.displayName}`:'Not signed in'} · {status?.settings?.enabled?'Automatic upload enabled':'Automatic upload paused'}</p>
  <div className="flex gap-3">
   <button className="bg-blue-600 text-white rounded px-4 py-2 disabled:opacity-50" disabled={busy||status?.signing_in} onClick={()=>run(async()=>setCode(await invoke('btbi_begin_signin')))}>Sign in to Microsoft</button>
   {status?.settings?.profile&&<button className="border rounded px-4 py-2" disabled={busy} onClick={()=>run(()=>invoke('btbi_configure',{destination:status.settings!.destination,enabled:!status.settings!.enabled}))}>{status.settings.enabled?'Pause uploads':'Resume uploads'}</button>}
   <button className="border rounded px-4 py-2" disabled={busy} onClick={()=>run(()=>invoke('btbi_retry'))}>Retry pending uploads</button>
  </div>
  {code&&status?.signing_in&&<p role="status">Open <a className="underline" href={code.verification_uri} target="_blank" rel="noreferrer">Microsoft sign-in</a> and enter <strong>{code.user_code}</strong>. Only continue if the displayed app is BTBI Meetily.</p>}
  {error&&<p role="alert" className="text-red-700">{error}</p>}
  {status?.notice&&<p role="status">{status.notice}</p>}
  <h3 className="font-semibold">Upload history</h3>
  {status?.entries.length===0?<p>No meetings queued yet.</p>:<ul className="space-y-2">{status?.entries.map(e=><li key={e.capture_id} className="border rounded p-3"><strong>{e.status}</strong> · <span className="font-mono text-sm">{e.capture_id.slice(0,8)}</span>{e.error&&<p className="text-red-700">{e.error}</p>}</li>)}</ul>}
 </section>;
}
