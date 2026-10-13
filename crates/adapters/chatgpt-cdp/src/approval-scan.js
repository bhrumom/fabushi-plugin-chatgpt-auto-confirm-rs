// Request identity is independent of button wording, render count and reply text.
const approvalScan = () => {
  const visible=e=>!!(e && e.getClientRects().length);
  const norm=v=>String(v||'').replace(/\s+/g,' ').trim();
  const excluded='[data-message-author-role="user"], [data-content-search-unit-key$=":user"], blockquote, pre, code, nav, aside';
  const label=e=>{const c=e.cloneNode(true);c.querySelectorAll('kbd,[aria-hidden="true"]').forEach(n=>n.remove());return norm(e.getAttribute('aria-label')||c.textContent).replace(/\s+(?:Esc|Enter|↵|⏎|⌄|▾)$/i,'').trim();};
  const allow=/^(?:allow|approve|允许|allow once|允许一次|approve once|仅允许本次|允许本次|允许本次会话|允许此会话|仅允许本次会话|allow(?: .+)? for this (?:session|conversation))$/i;
  const deny=/^(?:reject|deny|拒绝|拒绝授权)$/i;
  const own=(o,k)=>{const d=o && Object.getOwnPropertyDescriptor(o,k);return d && 'value' in d?d.value:undefined;};
  const hash=s=>{let a=2166136261,b=5381;for(let i=0;i<s.length;i++){a=Math.imul(a^s.charCodeAt(i),16777619);b=Math.imul(b,33)^s.charCodeAt(i);}return (a>>>0).toString(16)+(b>>>0).toString(16);};
  const users=[...document.querySelectorAll('[data-content-search-unit-key$=":user"],[data-message-author-role="user"]')];
  const latestUser=users.at(-1);
  const cards=new Set();
  for(const button of document.querySelectorAll('button,[role="button"]')){
    if(!visible(button)||button.closest(excluded)||!allow.test(label(button)))continue;
    for(let card=button.parentElement,n=0;card && n<6 && !card.matches('main,body');card=card.parentElement,n++){
      if([...card.querySelectorAll('button,[role="button"]')].some(b=>deny.test(label(b)))){cards.add(card);break;}
    }
  }
  for(const card of document.querySelectorAll('[data-approval-card],[data-tool-approval],[data-testid*="approval"],[data-testid*="authorization"]')){
    if(visible(card)&&!card.closest(excluded)&&![...cards].some(c=>card.contains(c)))cards.add(card);
  }
  return [...cards].filter(card=>visible(card)&&!card.closest(excluded)).map(card=>{
    // Old transcript cards before the current user turn are never pending work.
    const historical=latestUser && !!(card.compareDocumentPosition(latestUser)&Node.DOCUMENT_POSITION_FOLLOWING);
    let request='',anchor='',resolved=false;
    for(let node=card,n=0;node && n<10 && !node.matches('main,body');node=node.parentElement,n++){
      resolved ||= /^(?:approved|allowed|denied|resolved|completed)$/i.test(node.getAttribute('data-approval-state')||node.getAttribute('data-status')||'');
      for(const key of ['data-approval-request-id','data-request-id','data-tool-call-id'])if(!request && node.getAttribute(key))request=node.getAttribute(key);
      if(!anchor)anchor=node.getAttribute('data-message-id')||node.getAttribute('data-content-search-unit-key')||node.getAttribute('data-content-search-turn-key')||'';
      let fiber=own(node,Object.keys(node).find(k=>k.startsWith('__reactFiber$')));
      for(let i=0;fiber && i<8;i++,fiber=own(fiber,'return')){
        const props=own(fiber,'memoizedProps'),message=own(props,'message');
        if(!request)request=own(props,'approvalRequestId')||own(props,'toolCallId')||own(message,'allowTargetMessageId')||own(props,'allowTargetMessageId')||'';
        if(!anchor)anchor=own(message,'messageId')||own(props,'messageId')||'';
        resolved ||= !!own(props,'resolvedTargetMessageId');
      }
    }
    if(typeof request!=='string' && typeof request!=='number')request='';
    let payload=card;
    // Include the nearby permission description, never the whole changing reply.
    for(let n=0;n<2 && payload.parentElement && !payload.parentElement.matches('main,body,article,[data-content-search-turn-key]');n++){
      const parent=payload.parentElement;
      if(!parent.matches('form,[data-approval-card],[data-tool-approval],[data-testid*="approval"],[data-testid*="authorization"]'))break;
      if((parent.textContent||'').length>2500 || [...parent.querySelectorAll('button,[role="button"]')].filter(b=>deny.test(label(b))).length!==1)break;
      payload=parent;
    }
    const copy=payload.cloneNode(true);copy.querySelectorAll('button,[role="button"],kbd,[aria-hidden="true"]').forEach(e=>e.remove());
    const userAnchor=norm(latestUser?.textContent).match(/\[Fabushi:[^\]]+\]/)?.[0]||latestUser?.getAttribute('data-content-search-unit-key')||hash(norm(latestUser?.textContent));
    const key='approval-v2:'+hash(request ? 'request:'+request : 'turn:'+anchor+'|user:'+userAnchor+'|body:'+norm(copy.textContent));
    const controls=[...card.querySelectorAll('button,[role="button"]')].filter(b=>visible(b)&&!b.disabled&&b.getAttribute('aria-disabled')!=='true');
    return {card,key,resolved:resolved||!!historical,actionable:controls.some(b=>allow.test(label(b))||b.getAttribute('aria-haspopup')==='menu')};
  });
};
