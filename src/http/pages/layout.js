// Shared layout of the few HTML pages the server shows in a browser (e-mail links and the Google
// sign-in callback). No JavaScript, no external resource, inline styles only; served with the
// strict CSP below. Colours follow the game: dark wood, ivory, brass.

export const PAGE_CSP = "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

/**
 * HTML escaping for text and attribute values.
 * @param {unknown} s
 * @returns {string}
 */
export function escapeHtml(s) {
    return String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
}

const STYLE = `
:root { color-scheme: dark; }
* { box-sizing: border-box; }
body { margin: 0; min-height: 100vh; display: flex; align-items: center; justify-content: center;
  padding: 24px 16px; font: 17px/1.55 Georgia, "Times New Roman", serif; color: #efe6d2;
  background: #1b130d radial-gradient(ellipse at top, #3a2819 0%, #1b130d 70%); }
main { width: 100%; max-width: 440px; background: linear-gradient(#2c2017, #241a12); border: 1px solid #5a4330;
  border-radius: 10px; padding: 32px 28px; box-shadow: 0 18px 50px rgba(0,0,0,.55), inset 0 1px 0 rgba(255,240,210,.06); }
.brand { margin: 0 0 18px; font-size: 13px; letter-spacing: .18em; text-transform: uppercase; color: #c9a45c; }
h1 { margin: 0 0 14px; font-weight: normal; font-size: 26px; line-height: 1.25; color: #f6eedc; }
p { margin: 0 0 14px; color: #dcd0b8; }
.note { font-size: 14px; color: #a8987c; }
.error { background: rgba(160,50,40,.18); border: 1px solid #8a3a2e; color: #f0c8bd; border-radius: 6px; padding: 10px 12px; }
.ok { color: #cfe3b5; }
label { display: block; margin: 16px 0 6px; font-size: 15px; color: #dcd0b8; }
input[type=password] { width: 100%; padding: 11px 12px; font: inherit; color: #1f160f; background: #f3ead8;
  border: 1px solid #b89b6a; border-radius: 6px; }
input[type=password]:focus { outline: 2px solid #c9a45c; outline-offset: 1px; }
button { margin-top: 22px; width: 100%; padding: 12px 16px; font: inherit; font-size: 17px; cursor: pointer;
  color: #1f160f; background: linear-gradient(#f6eedc, #e4d6b8); border: 1px solid #b89b6a; border-radius: 6px; }
button:hover { background: linear-gradient(#fff8e8, #eadcbf); }
`;

/**
 * A complete page.
 * @param {{ serverName: string, title: string, body: string }} p `body` is trusted HTML
 * @returns {string}
 */
export function renderPage({ serverName, title, body }) {
    return '<!DOCTYPE html>\n<html lang="en"><head><meta charset="utf-8">' +
        '<meta name="viewport" content="width=device-width, initial-scale=1">' +
        '<meta name="referrer" content="no-referrer"><meta name="robots" content="noindex">' +
        `<title>${escapeHtml(title)} - ${escapeHtml(serverName)}</title><style>${STYLE}</style></head>` +
        `<body><main><p class="brand">${escapeHtml(serverName)}</p>${body}</main></body></html>\n`;
}

/**
 * A page with a heading and a message (results, errors).
 * @param {{ serverName: string, title: string, message: string, note?: string, tone?: 'ok'|'error'|'' }} p
 * @returns {string}
 */
export function renderMessage({ serverName, title, message, note = '', tone = '' }) {
    const cls = tone === 'error' ? ' class="error"' : tone === 'ok' ? ' class="ok"' : '';
    return renderPage({
        serverName,
        title,
        body: `<h1>${escapeHtml(title)}</h1><p${cls}>${escapeHtml(message)}</p>` + (note ? `<p class="note">${escapeHtml(note)}</p>` : ''),
    });
}
