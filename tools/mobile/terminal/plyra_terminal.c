/* Plyra PTY adapter: local user shell + upstream libvterm, protocol v1.
 * Structured launch arguments select the owned user process. Input is sent to its PTY. */
#define _DARWIN_C_SOURCE 1
#define _DEFAULT_SOURCE 1
#define _XOPEN_SOURCE 700
#include <errno.h>
#include <ctype.h>
#include <fcntl.h>
#include <poll.h>
#include <pwd.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#ifdef __APPLE__
#include <util.h>
#include <libproc.h>
#else
#include <pty.h>
#include <dirent.h>
#endif
#include "vterm.h"

#define COLS_MIN 10
#define COLS_MAX 240
#define ROWS_MIN 4
#define ROWS_MAX 100
#define CHUNK_MAX 65536
#define COMMAND_MAX 87500
#define OUTPUT_MAX (8u * 1024u * 1024u)
#define INPUT_MAX (256u * 1024u)
#define EVENT_MAX (1024u * 1024u)
#define SB_MAX 128
#define TITLE_MAX 1024
#define SESSION_PIDS_MAX 65536
#define HYPERLINK_CAPACITY 1024
#define URI_MAX 2048

typedef struct { char *p; size_t n, cap, off, max; bool failed; } Buf;
typedef struct { int cols; VTermScreenCell *cells; bool wrapped; } SavedLine;
typedef struct {
  VTerm *vt; VTermState *state; VTermScreen *screen;
  int rows, cols, master; pid_t child; bool dirty[ROWS_MAX];
  bool changed, full, alternate, cursor_visible, clear_history, closing;
  char title[TITLE_MAX + 1]; size_t title_len;
  int mouse_mode; bool focus_report, timeline;
  uint16_t current_link;
  char *uris[HYPERLINK_CAPACITY], osc[4097]; size_t osc_len;
  Buf output, input; SavedLine sb[SB_MAX]; int sb_head, sb_count; unsigned sb_dropped;
} App;
static volatile sig_atomic_t stop_requested;
static void on_signal(int sig) { (void)sig; stop_requested = 1; }
static void on_child_exit(int sig) { (void)sig; }
static int64_t now_ms(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return (int64_t)t.tv_sec * 1000 + t.tv_nsec / 1000000; }
static void buf_reset(Buf *b) { b->n = b->off = 0; b->failed = false; }
static bool append(Buf *b, const void *p, size_t n) {
  if (b->failed || n > b->max || b->n > b->max - n) { b->failed = true; return false; }
  if (b->n + n > b->cap) { size_t cap = b->cap ? b->cap : 4096; while (cap < b->n + n) cap *= 2; if (cap > b->max) cap = b->max; char *q = realloc(b->p, cap); if (!q) { b->failed = true; return false; } b->p = q; b->cap = cap; }
  memcpy(b->p + b->n, p, n); b->n += n; return true;
}
static void str(Buf *b, const char *s) { append(b, s, strlen(s)); }
static void num(Buf *b, long n) { char s[32]; snprintf(s, sizeof s, "%ld", n); str(b, s); }
static void compact(Buf *b) { if (b->off) { memmove(b->p, b->p + b->off, b->n - b->off); b->n -= b->off; b->off = 0; } }
static bool queue(App *a, Buf *event) {
  compact(&a->output);
  if (event->failed || event->n > EVENT_MAX || a->output.n > OUTPUT_MAX - event->n) { a->closing = true; return false; }
  return append(&a->output, event->p, event->n);
}
static void json_cp(Buf *b, uint32_t cp);
static void quote(Buf *b, const char *s, size_t n) {
  str(b, "\"");
  for (size_t i = 0; i < n;) {
    unsigned char c = (unsigned char)s[i++]; uint32_t cp = c; int rem = 0;
    if (c >= 0xc2 && c <= 0xdf) { cp = c & 31; rem = 1; }
    else if (c >= 0xe0 && c <= 0xef) { cp = c & 15; rem = 2; }
    else if (c >= 0xf0 && c <= 0xf4) { cp = c & 7; rem = 3; }
    else if (c >= 0x80) cp = 0xfffd;
    if (rem) {
      if (i + rem > n) { cp = 0xfffd; }
      else { size_t old = i; bool valid = true;
        for (int j = 0; j < rem; j++) { unsigned char x = (unsigned char)s[i]; if ((x & 0xc0) != 0x80) { valid = false; break; } cp = (cp << 6) | (x & 63); i++; }
        if (!valid) { i = old; cp = 0xfffd; }
        else if ((rem == 1 && cp < 0x80) || (rem == 2 && cp < 0x800) || (rem == 3 && cp < 0x10000)) cp = 0xfffd;
      }
    }
    if (cp == 0) str(b, "\\u0000"); else json_cp(b, cp);
  }
  str(b, "\"");
}
static void json_cp(Buf *b, uint32_t cp) {
  if (!cp || cp == (uint32_t)-1) cp = ' ';
  if (cp > 0x10ffff || (cp >= 0xd800 && cp <= 0xdfff)) cp = 0xfffd;
  if (cp == '"' || cp == '\\') { char x[2] = {'\\', (char)cp}; append(b, x, 2); }
  else if (cp < 32) { char x[7]; snprintf(x, sizeof x, "\\u%04x", cp); str(b, x); }
  else if (cp < 128) { char x = cp; append(b, &x, 1); }
  else if (cp < 0x800) { char x[2] = {(char)(0xc0 | (cp >> 6)), (char)(0x80 | (cp & 63))}; append(b, x, 2); }
  else if (cp < 0x10000) { char x[3] = {(char)(0xe0 | (cp >> 12)), (char)(0x80 | ((cp >> 6) & 63)), (char)(0x80 | (cp & 63))}; append(b, x, 3); }
  else { char x[4] = {(char)(0xf0 | (cp >> 18)), (char)(0x80 | ((cp >> 12) & 63)), (char)(0x80 | ((cp >> 6) & 63)), (char)(0x80 | (cp & 63))}; append(b, x, 4); }
}
static void error_event(App *a, const char *message) { Buf e = {.max=EVENT_MAX}; str(&e, "{\"type\":\"error\",\"message\":"); quote(&e, message, strlen(message)); str(&e, "}\n"); queue(a, &e); free(e.p); }
static void terminal_output(const char *s, size_t n, void *user) { App *a = user; compact(&a->input); if (!append(&a->input, s, n)) { error_event(a, "PTY input queue limit exceeded"); a->closing = true; } }
static int damage(VTermRect rect, void *user) {
  App *a = user;
  for (int row=rect.start_row; row<rect.end_row && row<a->rows; row++)
    if (row>=0) a->dirty[row]=true;
  a->changed=true;
  return 1;
}
static int move_rect(VTermRect dst, VTermRect src, void *user) {
  (void)src;
  return damage(dst,user);
}
static int cursor(VTermPos pos, VTermPos old, int visible, void *user) { (void)pos; (void)old; App *a = user; a->cursor_visible = !!visible; a->changed = true; return 1; }
static int prop(VTermProp p, VTermValue *v, void *user) {
  App *a = user;
  if (p == VTERM_PROP_ALTSCREEN) { a->alternate = !!v->boolean; a->full = true; }
  else if (p == VTERM_PROP_MOUSE) a->mouse_mode = v->number;
  else if (p == VTERM_PROP_FOCUSREPORT) a->focus_report = !!v->boolean;
  else if (p == VTERM_PROP_CURSORVISIBLE) a->cursor_visible = !!v->boolean;
  else if (p == VTERM_PROP_TITLE) { if (v->string.initial) a->title_len = 0; size_t n = v->string.len; if (n > TITLE_MAX - a->title_len) n = TITLE_MAX - a->title_len; memcpy(a->title + a->title_len, v->string.str, n); a->title_len += n; a->title[a->title_len] = 0; }
  a->changed = true; return 1;
}
static int screen_resize(int rows, int cols, void *user) { App *a = user; a->rows = rows; a->cols = cols; a->changed = a->full = true; return 1; }
static void sb_drop(App *a) { SavedLine *l = &a->sb[a->sb_head]; free(l->cells); l->cells = NULL; a->sb_head = (a->sb_head + 1) % SB_MAX; a->sb_count--; }
static void flush_history(App *a);
static int sb_push(int cols, const VTermScreenCell *cells, int wrapped, void *user) {
  App *a = user; if (cols < 1 || cols > COLS_MAX) return 0;
  if (a->sb_count >= 8) flush_history(a);
  if (a->closing || a->sb_count == SB_MAX) return 0;
  SavedLine *l = &a->sb[(a->sb_head + a->sb_count) % SB_MAX]; l->cells = malloc(sizeof(*cells) * cols); if (!l->cells) { a->closing = true; return 0; }
  memcpy(l->cells, cells, sizeof(*cells) * cols); l->cols = cols; l->wrapped = !!wrapped; a->sb_count++; return 1;
}
static int sb_clear(void *user) { App *a = user; while (a->sb_count) sb_drop(a); a->clear_history = true; a->changed = true; return 1; }
static const VTermScreenCallbacks callbacks = {.damage=damage,.moverect=move_rect,.movecursor=cursor,.settermprop=prop,.resize=screen_resize,.sb_pushline_wrapped=sb_push,.sb_clear=sb_clear};
static bool same_style(VTermScreenCell *x, VTermScreenCell *y, bool x_bg_default, bool y_bg_default) {
  return x_bg_default == y_bg_default && x->attrs.bold == y->attrs.bold && x->attrs.italic == y->attrs.italic && x->attrs.underline == y->attrs.underline && x->attrs.reverse == y->attrs.reverse && x->attrs.conceal == y->attrs.conceal && x->attrs.strike == y->attrs.strike && vterm_color_is_equal(&x->fg,&y->fg) && vterm_color_is_equal(&x->bg,&y->bg);
}
static void run_start(Buf *e, VTermScreenCell *c, const VTermScreenCell *original, bool bg_default) {
  char color[512]; snprintf(color, sizeof color, "{\"fg\":\"#%02x%02x%02x\",\"bg\":\"#%02x%02x%02x\",\"bg_default\":%s,\"fg_default\":%s,\"bold\":%s,\"italic\":%s,\"underline\":%s,\"inverse\":%s,\"strike\":%s", c->fg.rgb.red,c->fg.rgb.green,c->fg.rgb.blue,c->bg.rgb.red,c->bg.rgb.green,c->bg.rgb.blue,bg_default?"true":"false",VTERM_COLOR_IS_DEFAULT_FG(&original->fg)?"true":"false",c->attrs.bold?"true":"false",c->attrs.italic?"true":"false",c->attrs.underline?"true":"false",c->attrs.reverse?"true":"false",c->attrs.strike?"true":"false"); str(e,color);
  if (VTERM_COLOR_IS_INDEXED(&original->fg)) { str(e,",\"fg_index\":"); num(e,original->fg.indexed.idx); }
  if (VTERM_COLOR_IS_INDEXED(&original->bg)) { str(e,",\"bg_index\":"); num(e,original->bg.indexed.idx); }
  str(e,",\"text\":\"");
}
static bool ascii_cell(const VTermScreenCell *c) { return c->width == 1 && (!c->chars[0] || (c->chars[0] >= 32 && c->chars[0] <= 126)) && !c->chars[1]; }
static void resolve_cell_colors(App *a, VTermScreenCell *cell) {
  VTermColor foreground, background;
  vterm_state_get_default_colors(a->state,&foreground,&background);
  if (VTERM_COLOR_IS_DEFAULT_FG(&cell->fg)) cell->fg=foreground;
  if (VTERM_COLOR_IS_DEFAULT_BG(&cell->bg)) cell->bg=background;
  vterm_state_convert_color_to_rgb(a->state,&cell->fg);
  vterm_state_convert_color_to_rgb(a->state,&cell->bg);
}
static void line_json(App *a, Buf *e, VTermScreenCell *cells, int cols) {
  str(e,"{\"runs\":["); bool first=true; int i=0;
  while (i < cols) { VTermScreenCell c = cells[i]; if (c.width == 0 || c.chars[0] == (uint32_t)-1) { i++; continue; }
    VTermScreenCell original = c;
    uint16_t link = c.attrs.hyperlink;
    bool bg_default = VTERM_COLOR_IS_DEFAULT_BG(&c.bg);
    resolve_cell_colors(a,&c);
    if (!first) str(e,","); first=false; run_start(e,&c,&original,bg_default); int width=0;
    do { VTermScreenCell x=cells[i]; int w = x.width > 0 ? x.width : 1; if (w > 2 || w > cols-i) w=1;
      if (x.attrs.conceal) json_cp(e,' '); else if (!x.chars[0]) json_cp(e,' '); else for (int j=0;j<VTERM_MAX_CHARS_PER_CELL && x.chars[j];j++) json_cp(e,x.chars[j]);
      width+=w; i+=w; if (i>=cols || !ascii_cell(&x)) break;
      x=cells[i];
      if (!vterm_color_is_equal(&original.fg,&x.fg) || !vterm_color_is_equal(&original.bg,&x.bg)) break;
      resolve_cell_colors(a,&x); if ((x.attrs.hyperlink != link) || !ascii_cell(&x) || !same_style(&c,&x,bg_default,VTERM_COLOR_IS_DEFAULT_BG(&cells[i].bg))) break;
    } while (i<cols);
    str(e,"\",\"width\":"); num(e,width);
    if (link > 0 && link <= HYPERLINK_CAPACITY && a->uris[link-1]) { str(e,",\"hyperlink\":"); quote(e,a->uris[link-1],strlen(a->uris[link-1])); }
    str(e,"}");
  } str(e,"]}");
}
static int used_columns(const VTermScreenCell *cells, int cols) {
  while (cols > 0) {
    const VTermScreenCell *cell = &cells[cols - 1];
    if (cell->chars[0] || !VTERM_COLOR_IS_DEFAULT_BG(&cell->bg) || cell->attrs.reverse ||
        cell->attrs.underline || cell->attrs.strike || cell->attrs.hyperlink) break;
    cols--;
  }
  return cols;
}
static void flush_history(App *a) {
  if (a->clear_history) { Buf e={.max=EVENT_MAX}; str(&e,"{\"type\":\"scrollback_clear\"}\n"); queue(a,&e); free(e.p); a->clear_history=false; }
  for (int batch=0; a->sb_count && batch<16; batch++) {
    Buf e={.max=EVENT_MAX}; str(&e,"{\"type\":\"scrollback\",\"dropped\":"); num(&e,a->sb_dropped); a->sb_dropped=0; str(&e,",\"lines\":["); int n=a->sb_count<8?a->sb_count:8;
    for (int i=0;i<n;i++) {
      if(i)str(&e,",");
      SavedLine *l=&a->sb[a->sb_head];
      str(&e,"{\"wrapped\":"); str(&e,l->wrapped?"true":"false");
      str(&e,",\"used_columns\":"); num(&e,used_columns(l->cells,l->cols)); str(&e,",");
      Buf line={.max=EVENT_MAX}; line_json(a,&line,l->cells,l->cols);
      if(line.failed || line.n<2)e.failed=true; else append(&e,line.p+1,line.n-1);
      free(line.p); sb_drop(a);
    }
    str(&e,"]}\n"); queue(a,&e); free(e.p);
  }
}
static void emit_updates(App *a) {
  if (a->output.n-a->output.off > OUTPUT_MAX/4) return;
  flush_history(a);
  if (a->output.n-a->output.off > OUTPUT_MAX/4) return;
  if (!a->changed && !a->full) return;
  bool first=true; int row=0;
  while (row<a->rows) {
    Buf e={.max=EVENT_MAX}; VTermPos p; vterm_state_get_cursorpos(a->state,&p);
    str(&e,"{\"type\":\"screen\",\"full\":"); str(&e,a->full&&first?"true":"false"); str(&e,",\"cols\":"); num(&e,a->cols); str(&e,",\"rows\":"); num(&e,a->rows);
    str(&e,",\"cursor\":{\"row\":"); num(&e,p.row); str(&e,",\"col\":"); num(&e,p.col); str(&e,",\"visible\":"); str(&e,a->cursor_visible?"true":"false"); str(&e,"},\"alternate\":"); str(&e,a->alternate?"true":"false"); str(&e,",\"title\":"); quote(&e,a->title,a->title_len); str(&e,",\"mouse_mode\":"); num(&e,a->mouse_mode);
    str(&e,",\"focus_report\":"); str(&e,a->focus_report?"true":"false");
    str(&e,",\"lines\":[");
    int n=0;
    for (;row<a->rows && n<8;row++) { if(!a->full&&!a->dirty[row])continue; VTermScreenCell cells[COLS_MAX]; memset(cells,0,sizeof cells); for(int c=0;c<a->cols;c++)vterm_screen_get_cell(a->screen,(VTermPos){row,c},&cells[c]);
      if(n)str(&e,","); str(&e,"{\"row\":"); num(&e,row); str(&e,",\"runs\":"); Buf line={.max=EVENT_MAX}; line_json(a,&line,cells,a->cols); /* strip the single object wrapper */
      if(line.failed||line.n<10)e.failed=true; else append(&e,line.p+8,line.n-9);
      str(&e,",\"wrapped\":"); str(&e,row+1<a->rows && vterm_state_get_lineinfo(a->state,row+1)->continuation?"true":"false");
      str(&e,",\"used_columns\":"); num(&e,used_columns(cells,a->cols));
      str(&e,"}"); free(line.p); a->dirty[row]=false;n++;
    }
    str(&e,"]}\n"); queue(a,&e); free(e.p); first=false;
  }
  a->full=a->changed=false;
}
static int b64val(unsigned char c) { if(c>='A'&&c<='Z')return c-'A'; if(c>='a'&&c<='z')return c-'a'+26; if(c>='0'&&c<='9')return c-'0'+52; if(c=='+')return 62;if(c=='/')return 63;return -1; }
static bool decode64(const char *s, unsigned char *out, size_t *len) {
  size_t n=strlen(s); if(n%4 || n>4*((CHUNK_MAX+2)/3))return false; size_t k=0;
  for(size_t i=0;i<n;i+=4) { int a=b64val(s[i]),b=b64val(s[i+1]); int c=s[i+2]=='='?0:b64val(s[i+2]),d=s[i+3]=='='?0:b64val(s[i+3]); if(a<0||b<0||c<0||d<0)return false;
    bool p2=s[i+2]=='=',p3=s[i+3]=='='; if((p2&&!p3)||((p2||p3)&&i+4!=n)||(p2&&(b&15))||(!p2&&p3&&(c&3)))return false;
    if(k>=CHUNK_MAX)return false; out[k++]=(a<<2)|(b>>4); if(!p2){if(k>=CHUNK_MAX)return false;out[k++]=(b<<4)|(c>>2);} if(!p3){if(k>=CHUNK_MAX)return false;out[k++]=(c<<6)|d;}
  } *len=k; return true;
}
static VTermKey key_name(const char *s) {
  struct {const char *n;VTermKey k;} keys[]={{"Up",VTERM_KEY_UP},{"Down",VTERM_KEY_DOWN},{"Left",VTERM_KEY_LEFT},{"Right",VTERM_KEY_RIGHT},{"Home",VTERM_KEY_HOME},{"End",VTERM_KEY_END},{"PageUp",VTERM_KEY_PAGEUP},{"PageDown",VTERM_KEY_PAGEDOWN},{"Insert",VTERM_KEY_INS},{"Delete",VTERM_KEY_DEL},{"Backspace",VTERM_KEY_BACKSPACE},{"Tab",VTERM_KEY_TAB},{"Enter",VTERM_KEY_ENTER},{"Escape",VTERM_KEY_ESCAPE}};
  for(size_t i=0;i<sizeof keys/sizeof keys[0];i++)if(!strcmp(s,keys[i].n))return keys[i].k;
  int f=0; char tail; if(sscanf(s,"F%d%c",&f,&tail)==1&&f>=1&&f<=12)return VTERM_KEY_FUNCTION(f);return VTERM_KEY_NONE;
}
static bool pending_link(App *a, uint16_t id) {
  for (int index=0; index<a->sb_count; index++) {
    SavedLine *line=&a->sb[(a->sb_head+index)%SB_MAX];
    for (int col=0; col<line->cols; col++) if (line->cells[col].attrs.hyperlink==id) return true;
  }
  return false;
}
static int osc_event(int command, VTermStringFragment fragment, void *user) {
  App *a = user;
  if (command != 8 && command != 133) return 0;
  if (command == 133 && !a->timeline) return 1;
  if (fragment.initial) a->osc_len = 0;
  size_t length = fragment.len;
  if (length > sizeof(a->osc)-1-a->osc_len) length = sizeof(a->osc)-1-a->osc_len;
  memcpy(a->osc+a->osc_len,fragment.str,length); a->osc_len += length;
  if (!fragment.final) return 1;
  a->osc[a->osc_len] = 0;
  if (command == 8) {
    a->current_link = 0;
    vterm_screen_set_hyperlink(a->screen,0);
    char *uri = strchr(a->osc,';');
    if (!uri || !*++uri || strlen(uri)>URI_MAX) return 1;
    for (size_t i=0; uri[i]; i++) if ((unsigned char)uri[i]<32 || (unsigned char)uri[i]==127) return 1;
    int slot=-1;
    for (int i=0; i<HYPERLINK_CAPACITY; i++) {
      if (a->uris[i] && !strcmp(a->uris[i],uri)) {
        a->current_link=i+1;
        vterm_screen_set_hyperlink(a->screen,a->current_link);
        return 1;
      }
      if (!a->uris[i] && slot<0) slot=i;
    }
    if (slot<0) {
      for (int i=0; i<HYPERLINK_CAPACITY; i++) {
        if (!vterm_screen_has_hyperlink(a->screen,i+1) && !pending_link(a,i+1)) { slot=i; break; }
      }
    }
    if (slot>=0) {
      char *copy=strdup(uri);
      if (!copy) { a->closing=true; return 1; }
      free(a->uris[slot]); a->uris[slot]=copy;
      a->current_link=slot+1;
      vterm_screen_set_hyperlink(a->screen,a->current_link);
    }
  } else {
    const char *phase = a->osc[0]=='A'?"prompt":a->osc[0]=='C'?"start":a->osc[0]=='D'?"finish":NULL;
    if (!phase) return 1;
    Buf event={.max=EVENT_MAX};
    str(&event,"{\"type\":\"command\",\"phase\":"); quote(&event,phase,strlen(phase));
    const char *detail=a->osc[1]==';'?a->osc+2:"";
    str(&event,",\"detail\":"); quote(&event,detail,strlen(detail)>128?128:strlen(detail));
    str(&event,"}\n"); queue(a,&event); free(event.p);
  }
  return 1;
}
static const VTermStateFallbacks fallbacks={.osc=osc_event};
static bool parse_palette(const char *text, unsigned long colors[18]) {
  const char *cursor=text;
  for (int i=0; i<18; i++) {
    while (*cursor && isspace((unsigned char)*cursor)) cursor++;
    char token[7];
    for (int digit=0; digit<6; digit++) {
      if (!cursor[digit] || !isxdigit((unsigned char)cursor[digit])) return false;
      token[digit]=cursor[digit];
    }
    token[6]=0;
    cursor+=6;
    if (*cursor && !isspace((unsigned char)*cursor)) return false;
    colors[i]=strtoul(token,NULL,16);
  }
  while (*cursor && isspace((unsigned char)*cursor)) cursor++;
  return !*cursor;
}
static void apply_palette(App *a, const unsigned long colors[18]) {
  VTermColor foreground, background;
  vterm_color_rgb(&foreground,colors[0]>>16,colors[0]>>8,colors[0]);
  vterm_color_rgb(&background,colors[1]>>16,colors[1]>>8,colors[1]);
  vterm_state_set_default_colors(a->state,&foreground,&background);
  for (int i=0; i<16; i++) {
    unsigned long packed=colors[i+2];
    VTermColor color;
    vterm_color_rgb(&color,packed>>16,packed>>8,packed);
    vterm_state_set_palette_color(a->state,i,&color);
  }
  a->full=a->changed=true;
}
static void command(App *a,char *line) {
  if (!strncmp(line,"palette ",8)) {
    unsigned long colors[18];
    if (!parse_palette(line+8,colors)) { error_event(a,"Invalid terminal palette"); return; }
    apply_palette(a,colors);
    return;
  }
  if(!strcmp(line,"close")){a->closing=true;return;}
  if (!strncmp(line,"mouse ",6)) {
    int row,col,button,pressed,modifiers; char extra;
    if (sscanf(line+6,"%d %d %d %d %d %c",&row,&col,&button,&pressed,&modifiers,&extra)!=5 || row<0 || row>=a->rows || col<0 || col>=a->cols || button<0 || button>7 || pressed<0 || pressed>1 || modifiers<0 || modifiers>7) { error_event(a,"Invalid mouse event"); return; }
    if (a->mouse_mode) {
      vterm_mouse_move(a->vt,row,col,(VTermModifier)modifiers);
      if (button) vterm_mouse_button(a->vt,button,pressed,(VTermModifier)modifiers);
    }
    return;
  }
  if (!strcmp(line,"focus 1") || !strcmp(line,"focus 0")) {
    if (line[6]=='1') vterm_state_focus_in(a->state); else vterm_state_focus_out(a->state);
    return;
  }
  if(!strncmp(line,"input ",6)||!strncmp(line,"paste ",6)) { unsigned char out[CHUNK_MAX];size_t n=0;if(!decode64(line+6,out,&n)){error_event(a,"Invalid base64 or input exceeds 64 KiB");return;}
    compact(&a->input); if(a->input.n>INPUT_MAX-n-16){error_event(a,"PTY input queue is full");return;}
    if(line[0]=='p')vterm_keyboard_start_paste(a->vt); terminal_output((char*)out,n,a); if(line[0]=='p')vterm_keyboard_end_paste(a->vt);return;
  }
  if(!strncmp(line,"resize ",7)) { int c=0,r=0;char t; if(sscanf(line+7,"%d %d %c",&c,&r,&t)!=2||c<COLS_MIN||c>COLS_MAX||r<ROWS_MIN||r>ROWS_MAX){error_event(a,"Resize outside 10..240 columns / 4..100 rows");return;}
    if(c!=a->cols||r!=a->rows){struct winsize ws={.ws_row=r,.ws_col=c};if(ioctl(a->master,TIOCSWINSZ,&ws)<0){error_event(a,"PTY resize failed");return;}vterm_set_size(a->vt,r,c);a->full=a->changed=true;}return;
  }
  if(!strncmp(line,"key ",4)){char name[32]={0},tail;int m=0,n=sscanf(line+4,"%31s %d %c",name,&m,&tail);VTermKey k=key_name(name);if((n!=1&&n!=2)||m<0||m>7||k==VTERM_KEY_NONE){error_event(a,"Unknown terminal key or modifier");return;}vterm_keyboard_key(a->vt,k,(VTermModifier)m);return;}
  error_event(a,"Unknown command");
}
static const char *shell_path(void) { const char *s=getenv("SHELL");if(s&&s[0]=='/'&&access(s,X_OK)==0)return s;struct passwd *p=getpwuid(getuid());if(p&&p->pw_shell&&p->pw_shell[0]=='/'&&access(p->pw_shell,X_OK)==0)return p->pw_shell;return "/bin/sh"; }
static void signal_session(pid_t sid,int sig) {
  /* Only processes still inside the PTY child's new session are eligible. */
#ifdef __APPLE__
  int needed=proc_listpids(PROC_ALL_PIDS,0,NULL,0); if(needed<=0)return; if(needed>(int)(SESSION_PIDS_MAX*sizeof(pid_t)))needed=SESSION_PIDS_MAX*sizeof(pid_t);
  pid_t *p=malloc(needed);if(!p)return;int n=proc_listpids(PROC_ALL_PIDS,0,p,needed)/sizeof(pid_t);for(int i=0;i<n;i++)if(p[i]>1&&p[i]!=getpid()&&getsid(p[i])==sid)kill(p[i],sig);free(p);
#else
  DIR *d=opendir("/proc");if(!d)return;struct dirent *e;int n=0;while((e=readdir(d))&&n<SESSION_PIDS_MAX){char *end;long p=strtol(e->d_name,&end,10);if(!*e->d_name||*end||p<=1||p==getpid())continue;n++;if(getsid((pid_t)p)==sid)kill((pid_t)p,sig);}closedir(d);
#endif
}
static bool child_finished(pid_t child) {
  siginfo_t info = {0};
  int result;
  do {
    result = waitid(P_PID, (id_t)child, &info, WEXITED | WNOHANG | WNOWAIT);
  } while (result < 0 && errno == EINTR);
  return result == 0 && info.si_pid == child;
}
static void cleanup(App *a, int *status) {
  if (a->master >= 0) { close(a->master); a->master = -1; }
  if (a->child <= 0) return;
  siginfo_t info = {0};
  int result;
  do {
    result = waitid(P_PID, (id_t)a->child, &info, WEXITED | WNOHANG | WNOWAIT);
  } while (result < 0 && errno == EINTR);
  if (result < 0 && errno == ECHILD) return;
  // Keep the leader unreaped until every session-scoped signal is complete.
  signal_session(a->child, SIGHUP);
  kill(-a->child, SIGHUP);
  for (int phase = 0; phase < 3; phase++) {
    int sig = phase == 0 ? 0 : phase == 1 ? SIGTERM : SIGKILL;
    if (sig) { signal_session(a->child, sig); kill(-a->child, sig); }
    int64_t until = now_ms() + (phase == 0 ? 120 : phase == 1 ? 200 : 80);
    while (now_ms() < until) {
      struct timespec delay = {0, 10000000};
      nanosleep(&delay, NULL);
    }
  }
  kill(a->child, SIGKILL);
  while (waitpid(a->child, status, 0) < 0 && errno == EINTR) {}
}
static void flush_output(App *a, bool complete, const char *pending_line,
                         size_t pending_length, bool pending_dropped) {
  int64_t deadline = now_ms() + 300;
  char control[7];
  size_t used = pending_length < sizeof control ? pending_length : sizeof control;
  memcpy(control, pending_line, used);
  bool dropping = pending_dropped || pending_length > sizeof control;
  while (a->output.n > a->output.off && !stop_requested &&
         (complete || now_ms() < deadline)) {
    ssize_t written = write(STDOUT_FILENO, a->output.p + a->output.off,
                            a->output.n - a->output.off);
    if (written > 0) { a->output.off += written; continue; }
    if (written < 0 && errno != EAGAIN && errno != EINTR) break;
    struct pollfd fds[2] = {
      {STDOUT_FILENO, POLLOUT, 0},
      {complete ? STDIN_FILENO : -1, POLLIN, 0}
    };
    int result = poll(fds, 2, complete ? 100 : 10);
    if (result < 0 && errno != EINTR) break;
    if (fds[0].revents & (POLLHUP | POLLERR | POLLNVAL)) break;
    if (fds[1].revents & (POLLHUP | POLLERR | POLLNVAL)) break;
    if (fds[1].revents & POLLIN) {
      char bytes[1024];
      ssize_t count = read(STDIN_FILENO, bytes, sizeof bytes);
      if (count == 0) break;
      for (ssize_t i = 0; i < count; i++) {
        if (bytes[i] == '\n') {
          if (!dropping && (used == 5 || (used == 6 && control[5] == '\r')) &&
              !memcmp(control, "close", 5)) return;
          used = 0;
          dropping = false;
        } else if (used < sizeof control) control[used++] = bytes[i];
        else dropping = true;
      }
    }
  }
}
static bool nonblock(int fd){int f=fcntl(fd,F_GETFL);return f>=0&&fcntl(fd,F_SETFL,f|O_NONBLOCK)==0;}
int main(int argc,char **argv) {
  App a={.rows=24,.cols=80,.master=-1,.cursor_visible=true,.output={.max=OUTPUT_MAX},.input={.max=INPUT_MAX}};
  char cmd[COMMAND_MAX+1];
  size_t cmdlen = 0;
  bool dropping = false;
  char **exec_argv = NULL;
  bool login_shell = false, natural_exit = false;
  unsigned long palette[18] = {
    0xbbbbbb,0x000000,0x000000,0xbb0000,0x00bb00,0xbbbb00,0x0000bb,0xbb00bb,
    0x00bbbb,0xbbbbbb,0x555555,0xff5555,0x55ff55,0xffff55,0x5555ff,0xff55ff,0x55ffff,0xffffff
  };
  for (int i=1; i<argc;) {
    if (!strcmp(argv[i],"--exec")) { if (i+1>=argc || argv[i+1][0]!='/') return 64; exec_argv=argv+i+1; break; }
    if (i+1>=argc) return 64;
    if (!strcmp(argv[i],"--palette")) {
      if (!parse_palette(argv[i+1],palette)) return 64;
      i+=2;
      continue;
    }
    char *end; long value=strtol(argv[i+1],&end,10); if (*end) return 64;
    if (!strcmp(argv[i],"--cols") && value>=COLS_MIN && value<=COLS_MAX) a.cols=value;
    else if (!strcmp(argv[i],"--rows") && value>=ROWS_MIN && value<=ROWS_MAX) a.rows=value;
    else if (!strcmp(argv[i],"--login") && value>=0 && value<=1) login_shell=!!value;
    else if (!strcmp(argv[i],"--timeline") && value>=0 && value<=1) a.timeline=!!value;
    else return 64;
    i+=2;
  }
  signal(SIGPIPE,SIG_IGN);signal(SIGTERM,on_signal);signal(SIGINT,on_signal);signal(SIGHUP,on_signal);
  struct sigaction child_action = {.sa_handler=on_child_exit};
  sigemptyset(&child_action.sa_mask);
  sigaction(SIGCHLD,&child_action,NULL);
  a.vt=vterm_new(a.rows,a.cols);if(!a.vt)return 70;vterm_set_utf8(a.vt,1);a.state=vterm_obtain_state(a.vt);a.screen=vterm_obtain_screen(a.vt);vterm_screen_enable_altscreen(a.screen,1);vterm_screen_set_callbacks(a.screen,&callbacks,&a);vterm_screen_set_damage_merge(a.screen,VTERM_DAMAGE_ROW);vterm_screen_enable_reflow(a.screen,1);vterm_screen_set_unrecognised_fallbacks(a.screen,&fallbacks,&a);vterm_output_set_callback(a.vt,terminal_output,&a);
  apply_palette(&a,palette);
  vterm_screen_reset(a.screen,1);
  const char *shell=exec_argv?exec_argv[0]:shell_path();char *owned_shell=strdup(shell);if(!owned_shell)return 70;struct winsize ws={.ws_row=a.rows,.ws_col=a.cols};
  a.child=forkpty(&a.master,NULL,NULL,&ws);
  if(a.child<0){error_event(&a,"Unable to create a pseudo-terminal");goto fail;}
  if(!a.child){signal(SIGPIPE,SIG_DFL);signal(SIGTERM,SIG_DFL);signal(SIGINT,SIG_DFL);signal(SIGHUP,SIG_DFL);setenv("TERM","xterm-256color",1);setenv("COLORTERM","truecolor",1);if(!getenv("LANG")&&!getenv("LC_ALL"))setenv("LANG","C.UTF-8",1);const char *home=getenv("HOME");if(home&&*home)chdir(home);else {struct passwd *p=getpwuid(getuid());if(p&&p->pw_dir)chdir(p->pw_dir);}char *const args[]={owned_shell,"-i",NULL};char *const login_args[]={owned_shell,"-l","-i",NULL};execv(owned_shell,exec_argv?exec_argv:login_shell?login_args:args);dprintf(STDERR_FILENO,"Plyra: cannot execute shell: %s\r\n",strerror(errno));_exit(127);}
  if(!nonblock(a.master)||!nonblock(STDIN_FILENO)||!nonblock(STDOUT_FILENO)){a.closing=true;error_event(&a,"Unable to configure nonblocking pipes");}
  {Buf e={.max=EVENT_MAX};str(&e,"{\"type\":\"ready\",\"protocol\":1,\"pid\":");num(&e,a.child);str(&e,",\"shell\":");quote(&e,owned_shell,strlen(owned_shell));str(&e,",\"cols\":");num(&e,a.cols);str(&e,",\"rows\":");num(&e,a.rows);str(&e,"}\n");queue(&a,&e);free(e.p);}a.full=a.changed=true;
  bool child_done=false,master_done=false;int child_status=0,child_retirement=0;int64_t next_frame=0,child_finished_at=0;
  while(!a.closing&&!stop_requested){
    if (!child_done && child_finished(a.child)) {
      child_done = true;
      child_finished_at = now_ms();
      signal_session(a.child, SIGHUP);
      kill(-a.child, SIGHUP);
    }
    if (child_done && master_done) {
      vterm_screen_flush_damage(a.screen);
      emit_updates(&a);
      if (!a.changed && !a.full && !a.sb_count && !a.clear_history &&
          a.output.n == a.output.off) {
        natural_exit = true;
        break;
      }
    }
    if (child_done) {
      int phase = now_ms() - child_finished_at >= 400 ? 2 :
        now_ms() - child_finished_at >= 200 ? 1 : 0;
      if (phase > child_retirement) {
        int sig = phase == 1 ? SIGTERM : SIGKILL;
        signal_session(a.child, sig);
        kill(-a.child, sig);
        child_retirement = phase;
      }
    }
    int64_t now=now_ms();if(now>=next_frame){vterm_screen_flush_damage(a.screen);emit_updates(&a);next_frame=now+16;}
    bool can_read_pty = a.output.n-a.output.off < OUTPUT_MAX/4;
    short master_events = master_done || !can_read_pty ? 0 :
      (short)(POLLIN | ((a.input.n>a.input.off)?POLLOUT:0));
    struct pollfd f[3]={{STDIN_FILENO,POLLIN,0},
      {master_events?a.master:-1,master_events,0},
      {STDOUT_FILENO,(short)(a.output.n>a.output.off?POLLOUT:0),0}};
    bool update_pending = a.changed || a.full || a.sb_count || a.clear_history;
    int timeout = update_pending ? 16 : (child_done || master_done) ? 20 : -1;
    int pr=poll(f,3,timeout);if(pr<0){if(errno==EINTR)continue;a.closing=true;break;}
    if(f[2].revents&(POLLERR|POLLHUP|POLLNVAL)){a.closing=true;break;}
    if(f[2].revents&POLLOUT){ssize_t n=write(STDOUT_FILENO,a.output.p+a.output.off,a.output.n-a.output.off);if(n>0){a.output.off+=n;if(a.output.off==a.output.n)buf_reset(&a.output);}else if(n<0&&errno!=EAGAIN&&errno!=EINTR)a.closing=true;}
    if(f[1].revents&POLLOUT){ssize_t n=write(a.master,a.input.p+a.input.off,a.input.n-a.input.off);if(n>0){a.input.off+=n;if(a.input.off==a.input.n)buf_reset(&a.input);}else if(n<0&&errno!=EAGAIN&&errno!=EINTR)a.closing=true;}
    if(can_read_pty && (f[1].revents&(POLLIN|POLLHUP|POLLERR))){char out[1024];for(int budget=0;budget<8;budget++){ssize_t n=read(a.master,out,sizeof out);if(n>0){vterm_input_write(a.vt,out,n);if(a.output.n-a.output.off>=OUTPUT_MAX/4)break;}else{if(n==0||(n<0&&errno==EIO))master_done=true;break;}}}
    if(f[0].revents&(POLLIN|POLLHUP|POLLERR)){char chunk[8192];ssize_t n=read(STDIN_FILENO,chunk,sizeof chunk);if(n==0){a.closing=true;break;}if(n<0){if(errno!=EAGAIN&&errno!=EINTR)a.closing=true;}else for(ssize_t i=0;i<n;i++){unsigned char c=chunk[i];if(c=='\n'){if(dropping)error_event(&a,"Command exceeds protocol line bound");else{cmd[cmdlen]=0;if(cmdlen&&cmd[cmdlen-1]=='\r')cmd[--cmdlen]=0;command(&a,cmd);}cmdlen=0;dropping=false;if(a.closing)break;}else if(!c){dropping=true;}else if(cmdlen<COMMAND_MAX&&!dropping)cmd[cmdlen++]=c;else dropping=true;}}
  }
  vterm_screen_flush_damage(a.screen);emit_updates(&a);cleanup(&a,&child_status);
  {int code=child_done?(WIFEXITED(child_status)?WEXITSTATUS(child_status):WIFSIGNALED(child_status)?128+WTERMSIG(child_status):1):0;Buf e={.max=EVENT_MAX};str(&e,"{\"type\":\"exit\",\"code\":");num(&e,code);str(&e,"}\n");queue(&a,&e);free(e.p);}
fail:
  flush_output(&a, natural_exit, cmd, cmdlen, dropping);
  while(a.sb_count)sb_drop(&a);for(int i=0;i<HYPERLINK_CAPACITY;i++)free(a.uris[i]);free(a.output.p);free(a.input.p);free(owned_shell);vterm_free(a.vt);return 0;
}

// FVPROV1 31931 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNTgyNjM5NzY3ZmMwZWI1MjEwNDBiMzdjMWU0ZTQzNDIyMzEzODk1NjY5NDFjNTk3NzBlYWQ0OWM4M2U5ZGUzMSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNzUzOTk5MjUyNzlhNjUzYmZmZTlmNTc2NmE2NDgyOTcwMGFjZTIxOGY0OWM5YTNmMjcxYmNjZTNjYWViZWI2NTgyNDA5ZWJiOTI3NWU5NTg0M2NmYWM0MzFlNzY2ODRiYWVmNTYwZGExNDc2OWQxYjhmZmViMTRhYWIyZmZmMGQifV19XQ== FVPROV1-END
