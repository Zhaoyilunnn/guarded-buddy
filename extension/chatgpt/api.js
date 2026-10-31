// Provider calls stay in the extension; bearer credentials are never archived.
export async function request(url, token, fetcher = fetch) {
  for (let attempt = 0; attempt < 4; attempt++) {
    const response = await fetcher(url, {
      credentials: "include",
      headers: token ? { Authorization: `Bearer ${token}` } : {},
      signal: AbortSignal.timeout(30000),
    });
    if (response.status === 401 || response.status === 403) {
      throw new Error("Sign in to ChatGPT in this browser, then retry.");
    }
    if (response.status === 429 || response.status >= 500) {
      if (attempt === 3) throw new Error(`ChatGPT HTTP ${response.status}; retry later.`);
      const retry = Number(response.headers.get("retry-after"));
      await new Promise(resolve => setTimeout(resolve, Math.min(60000, Math.max(retry * 1000 || 0, 1000 * 2 ** attempt))));
      continue;
    }
    if (!response.ok) throw new Error(`ChatGPT HTTP ${response.status}`);
    return response.json();
  }
}

export async function login() {
  const session = await request("https://chatgpt.com/api/auth/session");
  if (!session.accessToken || !session.user?.id) throw new Error("ChatGPT login has no account ID or access token.");
  const bytes = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(session.user.id));
  const account = [...new Uint8Array(bytes)].map(b => b.toString(16).padStart(2, "0")).join("");
  return { token: session.accessToken, account };
}

export async function listPage(token, offset) {
  const page = await request(`https://chatgpt.com/backend-api/conversations?offset=${offset}&limit=100&order=updated`, token);
  if (!Array.isArray(page.items)) throw new Error("ChatGPT conversation list format changed.");
  return page;
}

export async function conversation(token, id) {
  const body = await request(`https://chatgpt.com/backend-api/conversation/${encodeURIComponent(id)}`, token);
  if (!body.mapping || !body.current_node) throw new Error("ChatGPT conversation format changed.");
  body.conversation_id = body.conversation_id || id;
  if (body.conversation_id !== id) throw new Error("ChatGPT returned a different conversation ID.");
  for (const key of ["accessToken", "access_token", "cookies", "authorization"]) delete body[key];
  return body;
}

export function revision(value) {
  if (typeof value === "number") return value;
  const milliseconds = Date.parse(value);
  return Number.isFinite(milliseconds) ? milliseconds / 1000 : 0;
}
