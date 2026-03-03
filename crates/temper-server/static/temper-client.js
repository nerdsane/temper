(function(g){
'use strict';
function n(u){return String(u||'').replace(/\/+$/,'');}
function k(id){return "('"+encodeURIComponent(String(id))+"')";}
function Temper(baseUrl,tenantId){
  this.baseUrl=n(baseUrl);this.tenantId=String(tenantId||'default');
  this._listeners=new Map();this._statusListeners=new Set();
  this._status='disconnected';this._es=null;this._manualDisconnect=false;
  this._onStateChange=this._onStateChange.bind(this);
}
var p=Temper.prototype;
p._emitStatus=function(s){
  if(this._status===s)return;this._status=s;this._statusListeners.forEach(function(cb){cb(s);});
};
p._request=async function(m,path,body){
  var r=await fetch(this.baseUrl+path,{method:m,headers:{'X-Tenant-Id':this.tenantId,'Content-Type':'application/json'},body:body===undefined?undefined:JSON.stringify(body)});
  var txt=await r.text();
  if(!r.ok)throw new Error(txt||('HTTP '+r.status));
  if(!txt)return null;
  try{return JSON.parse(txt);}catch(_){throw new Error('Expected JSON response');}
};
p.list=async function(entityType,options){
  var o=options||{},q=new URLSearchParams();
  if(o.filter!==undefined)q.set('$filter',o.filter);
  if(o.orderby!==undefined)q.set('$orderby',o.orderby);
  if(o.top!==undefined)q.set('$top',String(o.top));
  if(o.skip!==undefined)q.set('$skip',String(o.skip));
  if(o.select!==undefined)q.set('$select',o.select);
  var qs=q.toString();
  var d=await this._request('GET','/tdata/'+encodeURIComponent(entityType)+(qs?('?'+qs):''));
  return Array.isArray(d)?d:d&&Array.isArray(d.value)?d.value:[];
};
p.get=function(entityType,entityId){return this._request('GET','/tdata/'+encodeURIComponent(entityType)+k(entityId));};
p.create=function(entityType,payload){return this._request('POST','/tdata/'+encodeURIComponent(entityType),payload||{});};
p.action=function(entityType,entityId,actionName,payload){
  return this._request('POST','/tdata/'+encodeURIComponent(entityType)+k(entityId)+'/Temper.'+encodeURIComponent(actionName),payload);
};
p.patch=function(entityType,entityId,payload){
  return this._request('PATCH','/tdata/'+encodeURIComponent(entityType)+k(entityId),payload||{});
};
p._listenerCount=function(){var t=0;this._listeners.forEach(function(h){t+=h.size;});return t;};
p._onStateChange=function(evt){
  var d;try{d=JSON.parse(evt.data);}catch(_){return;}
  if(d&&d.tenant&&d.tenant!==this.tenantId)return;
  var all=this._listeners.get('*');if(all)all.forEach(function(cb){cb(d);});
  var typed=this._listeners.get(d.entity_type);if(typed)typed.forEach(function(cb){cb(d);});
};
p.connect=function(){
  if(this._es)return;
  if(!g.EventSource)throw new Error('EventSource not supported in this browser');
  this._manualDisconnect=false;this._emitStatus('connecting');
  var self=this,es=new EventSource(this.baseUrl+'/tdata/$events');this._es=es;
  es.addEventListener('state_change',this._onStateChange);
  es.onmessage=this._onStateChange;
  es.onopen=function(){self._emitStatus('connected');};
  es.onerror=function(){if(!self._es||self._manualDisconnect)return;self._emitStatus(es.readyState===EventSource.CLOSED?'disconnected':'reconnecting');};
};
p.disconnect=function(){
  this._manualDisconnect=true;
  if(this._es){this._es.close();this._es=null;}
  this._emitStatus('disconnected');
};
p.on=function(eventType,handler){
  if(typeof handler!=='function')throw new Error('Handler must be a function');
  var hs=this._listeners.get(eventType);
  if(!hs){hs=new Set();this._listeners.set(eventType,hs);}hs.add(handler);
  if(!this._es)this.connect();
  var self=this;return function(){self.off(eventType,handler);};
};
p.off=function(eventType,handler){
  var hs=this._listeners.get(eventType);if(!hs)return;
  hs.delete(handler);if(hs.size===0)this._listeners.delete(eventType);
  if(this._listenerCount()===0)this.disconnect();
};
p.onStatus=function(handler){
  if(typeof handler!=='function')throw new Error('Status handler must be a function');
  this._statusListeners.add(handler);handler(this._status);
  var self=this;return function(){self._statusListeners.delete(handler);};
};
g.Temper=Temper;
})(window);
