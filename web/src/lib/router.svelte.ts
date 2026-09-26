import { isRouterClick, matchRoute, type Route } from './router';

export const nav: { route: Route; url: string } = $state({
  route: matchRoute(location.pathname, location.search),
  url: location.pathname + location.search,
});

function sync(): void {
  nav.route = matchRoute(location.pathname, location.search);
  nav.url = location.pathname + location.search;
}

export function navigate(to: string, opts: { replace?: boolean } = {}): void {
  if (to === location.pathname + location.search) return;
  if (opts.replace) history.replaceState(null, '', to);
  else history.pushState(null, '', to);
  sync();
  window.scrollTo(0, 0);
}

/** Installs popstate + same-origin link interception. Returns an uninstaller. */
export function startRouter(): () => void {
  const onPop = (): void => sync();
  const onClick = (ev: MouseEvent): void => {
    const target = ev.target;
    if (!(target instanceof Element)) return;
    const anchor = target.closest('a');
    if (!anchor || !isRouterClick(ev, anchor, location.origin)) return;
    ev.preventDefault();
    const url = new URL(anchor.href);
    navigate(url.pathname + url.search);
  };
  window.addEventListener('popstate', onPop);
  document.addEventListener('click', onClick);
  return () => {
    window.removeEventListener('popstate', onPop);
    document.removeEventListener('click', onClick);
  };
}
