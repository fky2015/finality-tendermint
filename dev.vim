let SessionLoad = 1
let s:so_save = &g:so | let s:siso_save = &g:siso | setg so=0 siso=0 | setl so=-1 siso=-1
let v:this_session=expand("<sfile>:p")
silent only
silent tabonly
cd ~/code/ghq/github.com/fky2015/finality-tendermint
if expand('%') == '' && !&modified && line('$') <= 1 && getline(1) == ''
  let s:wipebuf = bufnr('%')
endif
let s:shortmess_save = &shortmess
if &shortmess =~ 'A'
  set shortmess=aoOA
else
  set shortmess=aoO
endif
badd +76 src/lib.rs
badd +713 ~/code/ghq/github.com/bit-substrate/leader-consensus/src/leader/voter/mod.rs
badd +160 ~/code/ghq/github.com/bit-substrate/leader-consensus/src/lib.rs
badd +11 Cargo.toml
badd +13 ~/code/ghq/github.com/bit-substrate/leader-consensus/Cargo.toml
badd +19 ~/code/ghq/github.com/bit-substrate/leader-consensus/src/leader/mod.rs
argglobal
%argdel
$argadd .
tabnew +setlocal\ bufhidden=wipe
tabrewind
edit src/lib.rs
argglobal
balt Cargo.toml
setlocal fdm=expr
setlocal fde=nvim_treesitter#foldexpr()
setlocal fmr={{{,}}}
setlocal fdi=#
setlocal fdl=3
setlocal fml=1
setlocal fdn=20
setlocal fen
34
normal! zo
let s:l = 76 - ((16 * winheight(0) + 22) / 45)
if s:l < 1 | let s:l = 1 | endif
keepjumps exe s:l
normal! zt
keepjumps 76
normal! 018|
tabnext
argglobal
enew
balt src/lib.rs
setlocal fdm=expr
setlocal fde=nvim_treesitter#foldexpr()
setlocal fmr={{{,}}}
setlocal fdi=#
setlocal fdl=0
setlocal fml=1
setlocal fdn=20
setlocal fen
if exists(':tcd') == 2 | tcd ~/code/ghq/github.com/bit-substrate/leader-consensus | endif
tabnext 1
if exists('s:wipebuf') && len(win_findbuf(s:wipebuf)) == 0 && getbufvar(s:wipebuf, '&buftype') isnot# 'terminal'
  silent exe 'bwipe ' . s:wipebuf
endif
unlet! s:wipebuf
set winheight=1 winwidth=20
let &shortmess = s:shortmess_save
let s:sx = expand("<sfile>:p:r")."x.vim"
if filereadable(s:sx)
  exe "source " . fnameescape(s:sx)
endif
let &g:so = s:so_save | let &g:siso = s:siso_save
set hlsearch
doautoall SessionLoadPost
unlet SessionLoad
" vim: set ft=vim :
