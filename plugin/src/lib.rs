#![feature(plugin_registrar, quote, rustc_private, custom_attribute, try_from)]

extern crate rustc_plugin;
extern crate syntax;

use rustc_plugin::registry::Registry;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::fs::{create_dir_all, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use syntax::ast::*;
use syntax::codemap::Span;
use syntax::ext::base::{Annotatable, ExtCtxt, SyntaxExtension};
use syntax::fold::{self, Folder};
use syntax::ptr::P;
use syntax::symbol::Symbol;
use syntax::util::small_vector::SmallVector;

#[plugin_registrar]
pub fn plugin_registrar(reg: &mut Registry) {
    reg.register_syntax_extension(
        Symbol::intern("mutate"),
        SyntaxExtension::MultiModifier(Box::new(mutator)),
    );
}

static TARGET_MUTAGEN : &'static str = "target/mutagen";
static MUTATIONS_LIST : &'static str = "mutations.txt";
static MUTATION_COUNT : AtomicUsize = AtomicUsize::new(0);

/// create a MutatorPlugin and let it fold the items/trait items/impl items
pub fn mutator(cx: &mut ExtCtxt, _span: Span, _mi: &MetaItem, a: Annotatable) -> Annotatable {
    // create target/mutagen path if it doesn't exist
    let mutagen_dir = if cx.root_path.ends_with("src") {
        cx.root_path.parent().unwrap_or(::std::path::Path::new("."))
    } else {
        cx.root_path.as_path()
    }.join(TARGET_MUTAGEN);
    if !mutagen_dir.exists() {
        create_dir_all(&mutagen_dir).unwrap();
    }
    let mutation_fpath = mutagen_dir.join(MUTATIONS_LIST);
    let mutation_file = if MUTATION_COUNT.compare_and_swap(0, 1, SeqCst) > 0 {
        OpenOptions::new().append(true).open(mutation_fpath)
    } else {
        File::create(mutation_fpath)
    }.unwrap();
    let mutations = BufWriter::new(mutation_file);
    let mut p = MutatorPlugin::new(cx, mutations, MUTATION_COUNT.load(SeqCst));
    let result = match a {
        Annotatable::Item(i) => {
            Annotatable::Item(p.fold_item(i).expect_one("expected exactly one item"))
        }
        Annotatable::TraitItem(i) => Annotatable::TraitItem(i.map(|i| {
            p.fold_trait_item(i).expect_one("expected exactly one item")
        })),
        Annotatable::ImplItem(i) => Annotatable::ImplItem(i.map(|i| {
            p.fold_impl_item(i).expect_one("expected exactly one item")
        })),
    };
    p.mutations.flush().unwrap();
    MUTATION_COUNT.store(p.current_count, SeqCst);
    result
}

/// information about the current method
struct MethodInfo {
    /// does the return type implement the Default trait (best effort)
    is_default: bool,
    /// which inputs have the same type as the output?
    have_output_type: Vec<Symbol>,
    /// which inputs have the same type and could be switched?
    /// TODO refs vs. values
    interchangeables: HashMap<Symbol, Vec<Symbol>>,
}

#[derive(Default)]
struct MutatorInfo {
    /// a stack of method infos
    method_infos: Vec<MethodInfo>,
    /// Self types for known impls
    self_tys: Vec<Ty>,
}

/// The MutatorPlugin
pub struct MutatorPlugin<'a, 'cx: 'a> {
    /// context for quoting
    cx: &'a mut ExtCtxt<'cx>,
    /// information about the context
    info: MutatorInfo,
    /// a sequence of mutations
    mutations: BufWriter<File>,
    /// the current mutation count, starting from 1
    current_count: usize,
}

impl<'a, 'cx> MutatorPlugin<'a, 'cx> {
    fn new(cx: &'a mut ExtCtxt<'cx>, mutations: BufWriter<File>, count: usize) -> Self {
        MutatorPlugin {
            cx,
            info: Default::default(),
            mutations,
            current_count: count,
        }
    }

    fn start_fn(&mut self, decl: &FnDecl) {
        let (is_default, out_ty) = match decl.output {
            FunctionRetTy::Default(_) => (true, None),
            FunctionRetTy::Ty(ref ty) => {
                (is_ty_default(ty, self.info.self_tys.last()), Some(&**ty))
            }
        };
        // arguments of output type
        let mut have_output_type = vec![];
        // add arguments of same type, so we can switch them?
        let mut argtypes: HashMap<Symbol, (Mutability, &Ty)> = HashMap::new();
        let mut typeargs: HashMap<(Mutability, &Ty), Vec<Symbol>> = HashMap::new();
        for arg in &decl.inputs {
            if let Some((name, mutability)) = get_pat_name_mut(&arg.pat) {
                argtypes.insert(name, (mutability, &*arg.ty));
                typeargs.entry((mutability, &arg.ty)).or_insert(vec![]).push(name);
                if Some(&*arg.ty) == out_ty {
                    have_output_type.push(name);
                }
            }
        }
        let mut interchangeables = HashMap::new();
        for (arg, mut_ty) in argtypes {
            if typeargs[&mut_ty].len() > 1 {
                interchangeables.insert(
                    arg,
                    typeargs[&mut_ty].iter().cloned().filter(|a| a != &arg).collect(),
                );
            }
        }
        self.info.method_infos.push(MethodInfo {
            is_default,
            have_output_type,
            interchangeables
        });
    }

    fn end_fn(&mut self) {
        let info = self.info.method_infos.pop();
        assert!(info.is_some());
    }

    fn start_impl(&mut self, ty: &Ty) {
        self.info.self_tys.push(ty.clone());
    }

    fn end_impl(&mut self) {
        let ty = self.info.self_tys.pop();
        assert!(ty.is_some());
    }
}

impl<'a, 'cx> Folder for MutatorPlugin<'a, 'cx> {
    fn fold_impl_item(&mut self, i: ImplItem) -> SmallVector<ImplItem> {
        let mut is_fn = false;
        if let ImplItemKind::Method(ref sig, _) = i.node {
            self.start_fn(&sig.decl);
            is_fn = true;
        }
        let item = fold::noop_fold_impl_item(i, self);
        if is_fn {
            self.end_fn();
        }
        item
    }

    fn fold_trait_item(&mut self, i: TraitItem) -> SmallVector<TraitItem> {
        SmallVector::one(match i {
            TraitItem {
                id,
                ident,
                attrs,
                generics,
                node: TraitItemKind::Method(sig, Some(block)),
                span,
                tokens
            } => {
                self.start_fn(&sig.decl);
                let ti = TraitItem {
                    id,
                    ident,
                    attrs,
                    generics,
                    node: TraitItemKind::Method(sig, Some(fold_first_block(block, self))),
                    span,
                    tokens
                };
                self.end_fn();
                ti
            },
            ti => ti
        })
    }

    fn fold_item_kind(&mut self, i: ItemKind) -> ItemKind {
        match i {
            ItemKind::Impl(unsafety, polarity, defaultness, generics, opt_trait_ref, ty, impl_items) => {
                self.start_impl( & ty);
                let k = ItemKind::Impl(unsafety, polarity, defaultness, generics, opt_trait_ref, ty, impl_items);
                self.end_impl();
                k
            },
            ItemKind::Fn(decl, unsafety, constness, abi, generics, block) => {
                self.start_fn(&decl);
                let k = ItemKind::Fn(decl, unsafety, constness, abi, generics, fold_first_block(block, self));
                self.end_fn();
                k
            },
            k => k
        }
    }

    fn fold_expr(&mut self, expr: P<Expr>) -> P<Expr> {
        expr.and_then(|expr| match expr {
            Expr {
                id,
                node: ExprKind::Binary(op, left, right),
                span,
                attrs,
            } => match op.node {
                BinOpKind::And => {
                    let n;
                    {
                        n = self.current_count;
                        add_mutations(
                            &self.cx,
                            &mut self.mutations,
                            &mut self.current_count,
                            expr.span,
                            &[
                                "replacing _ && _ with false",
                                "replacing _ && _ with true",
                                "replacing x && _ with x",
                                "replacing x && _ with !x",
                                "replacing x && y with x && !y",
                            ],
                        );
                    }
                    let left = left.map(|e| fold::noop_fold_expr(e, self));
                    let right = right.map(|e| fold::noop_fold_expr(e, self));
                    quote_expr!(self.cx, mutagen::and(|| $left, || $right, $n))
                }
                BinOpKind::Or => {
                    let n;
                    {
                        n = self.current_count;
                        add_mutations(
                            &self.cx,
                            &mut self.mutations,
                            &mut self.current_count,
                            expr.span,
                            &[
                                "replacing _ || _ with false",
                                "replacing _ || _ with true",
                                "replacing x || _ with x",
                                "replacing x || _ with !x",
                                "replacing x || y with x || !y",
                            ],
                        );
                    }
                    let left = left.map(|e| fold::noop_fold_expr(e, self));
                    let right = right.map(|e| fold::noop_fold_expr(e, self));
                    quote_expr!(self.cx, mutagen::or(|| $left, || $right, $n))
                }
                BinOpKind::Eq => {
                    let n;
                    {
                        n = self.current_count;
                        add_mutations(
                            &self.cx,
                            &mut self.mutations,
                            &mut self.current_count,
                            expr.span,
                            &[
                                "replacing _ == _ with false",
                                "replacing _ == _ with true",
                                "replacing x == y with x != y",
                            ],
                        );
                    }
                    let left = left.map(|e| fold::noop_fold_expr(e, self));
                    let right = right.map(|e| fold::noop_fold_expr(e, self));
                    quote_expr!(self.cx, mutagen::eq(|| $left, || $right, $n))
                }
                BinOpKind::Ne => {
                    let n;
                    {
                        n = self.current_count;
                        add_mutations(
                            &self.cx,
                            &mut self.mutations,
                            &mut self.current_count,
                            expr.span,
                            &[
                                "replacing _ != _ with false",
                                "replacing _ != _ with true",
                                "replacing x != y with x == y",
                            ],
                        );
                    }
                    let left = left.map(|e| fold::noop_fold_expr(e, self));
                    let right = right.map(|e| fold::noop_fold_expr(e, self));
                    quote_expr!(self.cx, mutagen::ne(|| $left, || $right, $n))
                }
                BinOpKind::Gt => {
                    let n;
                    {
                        n = self.current_count;
                        add_mutations(
                            &self.cx,
                            &mut self.mutations,
                            &mut self.current_count,
                            expr.span,
                            &[
                                "replacing _ > _ with false",
                                "replacing _ > _ with true",
                                "replacing x > y with x < y",
                                "replacing x > y with x <= y",
                                "replacing x > y with x >= y",
                                "replacing x > y with x == y",
                                "replacing x > y with x != y",
                            ],
                        );
                    }
                    let left = left.map(|e| fold::noop_fold_expr(e, self));
                    let right = right.map(|e| fold::noop_fold_expr(e, self));
                    quote_expr!(self.cx, mutagen::gt(|| $left, || $right, $n))
                }
                BinOpKind::Lt => {
                    let n;
                    {
                        n = self.current_count;
                        add_mutations(
                            &self.cx,
                            &mut self.mutations,
                            &mut self.current_count,
                            expr.span,
                            &[
                                "replacing _ < _ with false",
                                "replacing _ < _ with true",
                                "replacing x < y with x > y",
                                "replacing x < y with x >= y",
                                "replacing x < y with x <= y",
                                "replacing x < y with x == y",
                                "replacing x < y with x != y",
                            ],
                        );
                    }
                    let left = left.map(|e| fold::noop_fold_expr(e, self));
                    let right = right.map(|e| fold::noop_fold_expr(e, self));
                    quote_expr!(self.cx, mutagen::gt(|| $right, || $left, $n))
                }
                BinOpKind::Ge => {
                    let n;
                    {
                        n = self.current_count;
                        add_mutations(
                            &self.cx,
                            &mut self.mutations,
                            &mut self.current_count,
                            expr.span,
                            &[
                                "replacing _ >= _ with false",
                                "replacing _ >= _ with true",
                                "replacing x >= y with x < y",
                                "replacing x >= y with x <= y",
                                "replacing x >= y with x > y",
                                "replacing x >= y with x == y",
                                "replacing x >= y with x != y",
                            ],
                        );
                    }
                    let left = left.map(|e| fold::noop_fold_expr(e, self));
                    let right = right.map(|e| fold::noop_fold_expr(e, self));
                    quote_expr!(self.cx, mutagen::ge(|| $left, || $right, $n))
                }
                BinOpKind::Le => {
                    let n;
                    {
                        n = self.current_count;
                        add_mutations(
                            &self.cx,
                            &mut self.mutations,
                            &mut self.current_count,
                            expr.span,
                            &[
                                "replacing _ <= _ with false",
                                "replacing _ <= _ with true",
                                "replacing x <= y with x > y",
                                "replacing x <= y with x >= y",
                                "replacing x <= y with x < y",
                                "replacing x <= y with x == y",
                                "replacing x <= y with x != y",
                            ],
                        );
                    }
                    let left = left.map(|e| fold::noop_fold_expr(e, self));
                    let right = right.map(|e| fold::noop_fold_expr(e, self));
                    quote_expr!(self.cx, mutagen::ge(|| $right, || $left, $n))
                }
                _ => P(fold::noop_fold_expr(
                    Expr {
                        id,
                        node: ExprKind::Binary(op, left, right),
                        span,
                        attrs,
                    },
                    self,
                )),
            },
            Expr {
                id,
                node: ExprKind::If(cond, then, opt_else),
                span,
                attrs,
            } => {
                let n;
                {
                    n = self.current_count;
                    add_mutations(
                        &self.cx,
                        &mut self.mutations,
                        &mut self.current_count,
                        cond.span,
                        &[
                            "replacing if condition with false",
                            "replacing if condition with true",
                            "inverting if condition",
                        ]
                    );
                }
                let cond = cond.map(|e| fold::noop_fold_expr(e, self));
                let then = fold::noop_fold_block(then, self);
                let opt_else = opt_else.map(|p_else| p_else.map(|e| fold::noop_fold_expr(e, self)));
                let mut_cond = quote_expr!(self.cx, mutagen::t($cond, $n));
                P(Expr {
                    id,
                    node: ExprKind::If(mut_cond, then, opt_else),
                    span,
                    attrs
                })
            }
            Expr {
                id,
                node: ExprKind::While(cond, block, opt_label),
                span,
                attrs,
            } => {
                let n;
                {
                    n = self.current_count;
                    add_mutations(
                        &self.cx,
                        &mut self.mutations,
                        &mut self.current_count,
                        cond.span,
                        &["replacing while condition with false"]
                    );
                }
                let cond = cond.map(|e| fold::noop_fold_expr(e, self));
                let block = fold::noop_fold_block(block, self);
                let mut_cond = quote_expr!(self.cx, mutagen::w($cond, $n));
                P(Expr {
                    id,
                    node: ExprKind::While(mut_cond, block, opt_label),
                    span,
                    attrs
                })
            }
            e => P(fold::noop_fold_expr(e, self)),
        }) //TODO: more expr mutations
    }
}

fn fold_first_block(block: P<Block>, m: &mut MutatorPlugin) -> P<Block> {
    let mut pre_stmts = vec![];
    {
        let MutatorPlugin {
            ref mut cx,
            ref info,
            ref mut mutations,
            ref mut current_count,
        } = *m;
        if let Some(&MethodInfo {
            is_default,
            ref have_output_type,
            ref interchangeables,
        }) = info.method_infos.last() {
            if is_default {
                let n = *current_count;
                add_mutations(
                    cx,
                    mutations,
                    current_count,
                    block.span,
                    &["insert return default()"],
                );
                pre_stmts.push(
                    quote_stmt!(cx,
                if mutagen::now($n) { return Default::default(); })
                        .unwrap(),
                );
            }
            for name in have_output_type {
                let n = *current_count;
                let ident = name.to_ident();
                add_mutations(
                    cx,
                    mutations,
                    current_count,
                    block.span,
                    &[&format!("insert return {}", name)],
                );
                pre_stmts.push(
                    quote_stmt!(cx,
                if mutagen::now($n) { return $ident; })
                        .unwrap(),
                );
            }
            //TODO: switch interchangeables, need mutability info, too
            //for name in method_info.interchangeables { }
        }
    }
    if pre_stmts.is_empty() {
        fold::noop_fold_block(block, m)
    } else {
        block.map(
            |Block {
                 stmts,
                 id,
                 rules,
                 span,
                 recovered,
             }| {
                let mut newstmts: Vec<Stmt> = Vec::with_capacity(pre_stmts.len() + stmts.len());
                newstmts.extend(pre_stmts);
                newstmts.extend(
                    stmts
                        .into_iter()
                        .flat_map(|s| fold::noop_fold_stmt(s, m)),
                );
                Block {
                    stmts: newstmts,
                    id,
                    rules,
                    span,
                    recovered,
                }
            },
        )
    }
}

fn add_mutations(
    cx: &ExtCtxt,
    mutations: &mut BufWriter<File>,
    count: &mut usize,
    span: Span,
    descriptions: &[&str],
) {
    let span_desc = cx.codemap().span_to_string(span);
    for desc in descriptions {
        writeln!(mutations, "{} @ {}", desc, span_desc).unwrap()
    }
    *count += descriptions.len();
}

fn get_pat_name_mut(pat: &Pat) -> Option<(Symbol, Mutability)> {
    if let PatKind::Ident(mode, i, _) = pat.node {
        Some((i.node.name, match mode { BindingMode::ByRef(m) | BindingMode::ByValue(m) => m }))
    } else {
        None
    }
}

static ALWAYS_DEFAULT: &[&[&str]] = &[
    &["u8"],
    &["u16"],
    &["u32"],
    &["u64"],
    &["u128"],
    &["usize"],
    &["i8"],
    &["i16"],
    &["i32"],
    &["i64"],
    &["i128"],
    &["isize"],
    &["vec", "Vec"],
    &["option", "Option"],
    &["char"],
    &["str"],
    &["string", "String"],
    &["BTreeMap"],
    &["BTreeSet"],
    &["HashMap"],
    &["HashSet"],
    &["vec_deque", "VecDeque"],
    &["linked_list", "LinkedList"],
    &["heap", "Heap"],
    &["BinaryHeap"],
    &["time", "Duration"],
    &["iter", "Empty"],
    &["fmt", "Error"],
    &["hash", "SipHasher"],
    &["hash", "SipHasher24"],
    &["hash", "BuildHasherDefault"],
    &["collections", "hash_map", "DefaultHasher"],
    &["collections", "hash_map", "RandomState"],
    &["ffi", "CStr"],
    &["ffi", "CString"],
    &["ffi", "OsStr"],
    &["ffi", "OsString"],
    &["path", "PathBuf"],
    &["sync", "CondVar"],
];

static DEFAULT_IF_ARG: &[&[&str]] = &[
    &["boxed", "Box"],
    &["rc", "Rc"],
    &["rc", "Weak"],
    &["arc", "Arc"],
    &["arc", "Weak"],
    &["cell", "Cell"],
    &["cell", "RefCell"],
    &["cell", "UnsafeCell"],
    &["num", "Wrapping"],
    &["sync", "atomic", "AtomicPtr"],
    &["sync", "atomic", "AtomicBool"],
    &["sync", "atomic", "AtomicU8"],
    &["sync", "atomic", "AtomicU16"],
    &["sync", "atomic", "AtomicU32"],
    &["sync", "atomic", "AtomicU64"],
    &["sync", "atomic", "AtomicUsize"],
    &["sync", "atomic", "AtomicI8"],
    &["sync", "atomic", "AtomicI16"],
    &["sync", "atomic", "AtomicI32"],
    &["sync", "atomic", "AtomicI64"],
    &["sync", "atomic", "AtomicIsize"],
    &["sync", "Mutex"],
    &["sync", "RwLock"],
    &["mem", "ManuallyDrop"],
];

fn is_ty_default(ty: &Ty, self_ty: Option<&Ty>) -> bool {
    match ty.node {
        TyKind::Slice(_) | TyKind::Never => true,
        TyKind::Rptr(_lt, MutTy { ty: ref pty, .. }) => match pty.node {
            TyKind::Slice(_) => true,
            TyKind::Path(_, ref ty_path) => match_path(ty_path, &["str"]),
            _ => false,
        },
        TyKind::Paren(ref t) => is_ty_default(t, self_ty),
        TyKind::Array(ref inner, ref len) => {
            is_ty_default(inner, self_ty) && get_lit(len).map_or(false, |n| n <= 32)
        }
        TyKind::Tup(ref inners) => {
            inners.len() <= 12 && inners.iter().all(|t| is_ty_default(&*t, self_ty))
        }
        TyKind::Path(ref _qself, ref ty_path) => is_path_default(ty_path, self_ty),
        TyKind::TraitObject(ref bounds, _) | TyKind::ImplTrait(ref bounds) => {
            bounds.iter().any(|bound| {
                if let TraitTyParamBound(ref poly_trait, _) = *bound {
                    poly_trait
                        .trait_ref
                        .path
                        .segments
                        .last()
                        .map_or(false, |s| s.identifier.name == "Default")
                } else {
                    false
                }
            })
        }
        TyKind::ImplicitSelf => self_ty.map_or(false, |t| is_ty_default(t, None)),
        TyKind::Typeof(ref expr) => is_expr_default(expr, self_ty),
        _ => false,
    }
}

fn is_expr_default(expr: &Expr, self_ty: Option<&Ty>) -> bool {
    match expr.node {
        ExprKind::Path(_, ref path) => is_path_default(path, self_ty),
        ExprKind::Paren(ref e) => is_expr_default(e, self_ty),
        ExprKind::AddrOf(_, ref e) => match e.node {
            ExprKind::Array(ref exprs) => exprs.len() == 1,
            ExprKind::Path(_, ref path) => match_path(path, &["str"]),
            _ => false,
        },
        ExprKind::Repeat(ref e, ref len) => {
            is_expr_default(e, self_ty) && get_lit(len).map_or(false, |n| n <= 32)
        }
        ExprKind::Array(ref exprs) => exprs.len() == 1, // = Slice
        ExprKind::Tup(ref exprs) => {
            exprs.len() <= 12 && exprs.iter().all(|e| is_expr_default(e, self_ty))
        }
        _ => false,
    }
}

fn is_path_default(ty_path: &Path, self_ty: Option<&Ty>) -> bool {
    if ALWAYS_DEFAULT.iter().any(|p| match_path(ty_path, p)) {
        return true;
    }
    for path in DEFAULT_IF_ARG {
        if match_path(ty_path, path) {
            return ty_path.segments.last().map_or(false, |s| {
                s.parameters.as_ref().map_or(false, |p| {
                    if let AngleBracketed(ref data) = **p {
                        data.types.len() == 1 && is_ty_default(&*data.types[0], self_ty)
                    } else {
                        false
                    }
                })
            });
        }
    }
    // TODO: Cow
    false
}

fn match_path(path: &Path, pat: &[&str]) -> bool {
    path.segments
        .iter()
        .rev()
        .zip(pat.iter().rev())
        .all(|(a, b)| &a.identifier.name == b)
}

fn get_lit(expr: &Expr) -> Option<usize> {
    if let ExprKind::Lit(ref lit) = expr.node {
        if let LitKind::Int(val, _) = lit.node {
            return usize::try_from(val).ok();
        }
    }
    None
}
