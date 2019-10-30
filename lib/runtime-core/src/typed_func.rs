use crate::{
    error::RuntimeError,
    export::{Context, Export, FuncPointer},
    import::IsExport,
    types::{FuncSig, NativeWasmType, Type, WasmExternType},
    vm,
};
use std::{
    any::Any,
    convert::Infallible,
    ffi::c_void,
    fmt,
    marker::PhantomData,
    mem, panic,
    ptr::{self, NonNull},
    sync::Arc,
};

#[repr(C)]
pub enum WasmTrapInfo {
    Unreachable = 0,
    IncorrectCallIndirectSignature = 1,
    MemoryOutOfBounds = 2,
    CallIndirectOOB = 3,
    IllegalArithmetic = 4,
    MisalignedAtomicAccess = 5,
    Unknown,
}

impl fmt::Display for WasmTrapInfo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                WasmTrapInfo::Unreachable => "unreachable",
                WasmTrapInfo::IncorrectCallIndirectSignature => {
                    "incorrect `call_indirect` signature"
                }
                WasmTrapInfo::MemoryOutOfBounds => "memory out-of-bounds access",
                WasmTrapInfo::CallIndirectOOB => "`call_indirect` out-of-bounds",
                WasmTrapInfo::IllegalArithmetic => "illegal arithmetic operation",
                WasmTrapInfo::MisalignedAtomicAccess => "misaligned atomic access",
                WasmTrapInfo::Unknown => "unknown",
            }
        )
    }
}

/// This is just an empty trait to constrict that types that
/// can be put into the third/fourth (depending if you include lifetimes)
/// of the `Func` struct.
pub trait Kind {}

pub type Trampoline = unsafe extern "C" fn(*mut vm::Ctx, NonNull<vm::Func>, *const u64, *mut u64);
pub type Invoke = unsafe extern "C" fn(
    Trampoline,
    *mut vm::Ctx,
    NonNull<vm::Func>,
    *const u64,
    *mut u64,
    *mut WasmTrapInfo,
    *mut Option<Box<dyn Any>>,
    Option<NonNull<c_void>>,
) -> bool;

/// TODO(lachlan): Naming TBD.
/// This contains the trampoline and invoke functions for a specific signature,
/// as well as the environment that the invoke function may or may not require.
#[derive(Copy, Clone)]
pub struct Wasm {
    pub(crate) trampoline: Trampoline,
    pub(crate) invoke: Invoke,
    pub(crate) invoke_env: Option<NonNull<c_void>>,
}

impl Wasm {
    pub unsafe fn from_raw_parts(
        trampoline: Trampoline,
        invoke: Invoke,
        invoke_env: Option<NonNull<c_void>>,
    ) -> Self {
        Self {
            trampoline,
            invoke,
            invoke_env,
        }
    }
}

/// This type, as part of the `Func` type signature, represents a function that is created
/// by the host.
pub struct Host(());

impl Kind for Wasm {}
impl Kind for Host {}

/// Represents a list of WebAssembly values.
pub trait WasmTypeList {
    type CStruct;

    type RetArray: AsMut<[u64]>;

    /// Construct `Self` based on an array of returned values.
    fn from_ret_array(array: Self::RetArray) -> Self;

    /// Generates an empty array that will hold the returned values of
    /// the WebAssembly function.
    fn empty_ret_array() -> Self::RetArray;

    /// Transforms C values into Rust values.
    fn from_c_struct(c_struct: Self::CStruct) -> Self;

    /// Transforms Rust values into C values.
    fn into_c_struct(self) -> Self::CStruct;

    /// Get types of the current values.
    fn types() -> &'static [Type];

    /// This method is used to distribute the values onto a function,
    /// e.g. `(1, 2).call(func, …)`. This form is unlikely to be used
    /// directly in the code, see the `Func:call` implementation.
    unsafe fn call<Rets>(
        self,
        f: NonNull<vm::Func>,
        wasm: Wasm,
        ctx: *mut vm::Ctx,
    ) -> Result<Rets, RuntimeError>
    where
        Rets: WasmTypeList;
}

/// Empty trait to specify the kind of `ExternalFunction`: With or
/// without a `vm::Ctx` argument.
pub trait ExternalFunctionKind {}

pub struct ExplicitVmCtx {}
pub struct ImplicitVmCtx {}

impl ExternalFunctionKind for ExplicitVmCtx {}
impl ExternalFunctionKind for ImplicitVmCtx {}

/// Represents a function that can be converted to a `vm::Func`
/// (function pointer) that can be called within WebAssembly.
pub trait ExternalFunction<Kind, Args, Rets>
where
    Kind: ExternalFunctionKind,
    Args: WasmTypeList,
    Rets: WasmTypeList,
{
    fn to_raw(&self) -> NonNull<vm::Func>;
}

pub trait TrapEarly<Rets>
where
    Rets: WasmTypeList,
{
    type Error: 'static;
    fn report(self) -> Result<Rets, Self::Error>;
}

impl<Rets> TrapEarly<Rets> for Rets
where
    Rets: WasmTypeList,
{
    type Error = Infallible;
    fn report(self) -> Result<Rets, Infallible> {
        Ok(self)
    }
}

impl<Rets, E> TrapEarly<Rets> for Result<Rets, E>
where
    Rets: WasmTypeList,
    E: 'static,
{
    type Error = E;
    fn report(self) -> Result<Rets, E> {
        self
    }
}

/// Represents a function that can be used by WebAssembly.
pub struct Func<'a, Args = (), Rets = (), Inner: Kind = Wasm> {
    inner: Inner,
    f: NonNull<vm::Func>,
    ctx: *mut vm::Ctx,
    _phantom: PhantomData<(&'a (), Args, Rets)>,
}

unsafe impl<'a, Args, Rets> Send for Func<'a, Args, Rets, Wasm> {}
unsafe impl<'a, Args, Rets> Send for Func<'a, Args, Rets, Host> {}

impl<'a, Args, Rets> Func<'a, Args, Rets, Wasm>
where
    Args: WasmTypeList,
    Rets: WasmTypeList,
{
    pub(crate) unsafe fn from_raw_parts(
        inner: Wasm,
        f: NonNull<vm::Func>,
        ctx: *mut vm::Ctx,
    ) -> Func<'a, Args, Rets, Wasm> {
        Func {
            inner,
            f,
            ctx,
            _phantom: PhantomData,
        }
    }

    pub fn get_vm_func(&self) -> NonNull<vm::Func> {
        self.f
    }
}

impl<'a, Args, Rets> Func<'a, Args, Rets, Host>
where
    Args: WasmTypeList,
    Rets: WasmTypeList,
{
    pub fn new<F, Kind>(f: F) -> Func<'a, Args, Rets, Host>
    where
        Kind: ExternalFunctionKind,
        F: ExternalFunction<Kind, Args, Rets>,
    {
        Func {
            inner: Host(()),
            f: f.to_raw(),
            ctx: ptr::null_mut(),
            _phantom: PhantomData,
        }
    }
}

impl<'a, Args, Rets, Inner> Func<'a, Args, Rets, Inner>
where
    Args: WasmTypeList,
    Rets: WasmTypeList,
    Inner: Kind,
{
    /// Returns the types of the function inputs.
    pub fn params(&self) -> &'static [Type] {
        Args::types()
    }

    /// Returns the types of the function outputs.
    pub fn returns(&self) -> &'static [Type] {
        Rets::types()
    }
}

impl WasmTypeList for Infallible {
    type CStruct = Infallible;
    type RetArray = [u64; 0];

    fn from_ret_array(_: Self::RetArray) -> Self {
        unreachable!()
    }

    fn empty_ret_array() -> Self::RetArray {
        unreachable!()
    }

    fn from_c_struct(_: Self::CStruct) -> Self {
        unreachable!()
    }

    fn into_c_struct(self) -> Self::CStruct {
        unreachable!()
    }

    fn types() -> &'static [Type] {
        &[]
    }

    #[allow(non_snake_case)]
    unsafe fn call<Rets>(
        self,
        _: NonNull<vm::Func>,
        _: Wasm,
        _: *mut vm::Ctx,
    ) -> Result<Rets, RuntimeError>
    where
        Rets: WasmTypeList,
    {
        unreachable!()
    }
}

impl<A> WasmTypeList for (A,)
where
    A: WasmExternType,
{
    type CStruct = S1<A>;
    type RetArray = [u64; 1];

    fn from_ret_array(array: Self::RetArray) -> Self {
        (WasmExternType::from_native(NativeWasmType::from_binary(
            array[0],
        )),)
    }

    fn empty_ret_array() -> Self::RetArray {
        [0u64]
    }

    fn from_c_struct(c_struct: Self::CStruct) -> Self {
        let S1(a) = c_struct;
        (WasmExternType::from_native(a),)
    }

    fn into_c_struct(self) -> Self::CStruct {
        #[allow(unused_parens, non_snake_case)]
        let (a,) = self;
        S1(WasmExternType::to_native(a))
    }

    fn types() -> &'static [Type] {
        &[A::Native::TYPE]
    }

    #[allow(non_snake_case)]
    unsafe fn call<Rets>(
        self,
        f: NonNull<vm::Func>,
        wasm: Wasm,
        ctx: *mut vm::Ctx,
    ) -> Result<Rets, RuntimeError>
    where
        Rets: WasmTypeList,
    {
        let (a,) = self;
        let args = [a.to_native().to_binary()];
        let mut rets = Rets::empty_ret_array();
        let mut trap = WasmTrapInfo::Unknown;
        let mut user_error = None;

        if (wasm.invoke)(
            wasm.trampoline,
            ctx,
            f,
            args.as_ptr(),
            rets.as_mut().as_mut_ptr(),
            &mut trap,
            &mut user_error,
            wasm.invoke_env,
        ) {
            Ok(Rets::from_ret_array(rets))
        } else {
            if let Some(data) = user_error {
                Err(RuntimeError::Error { data })
            } else {
                Err(RuntimeError::Trap {
                    msg: trap.to_string().into(),
                })
            }
        }
    }
}

impl<'a, A: WasmExternType, Rets> Func<'a, (A,), Rets, Wasm>
where
    Rets: WasmTypeList,
{
    pub fn call(&self, a: A) -> Result<Rets, RuntimeError> {
        unsafe { <A as WasmTypeList>::call(a, self.f, self.inner, self.ctx) }
    }
}

macro_rules! impl_traits {
    ( [$repr:ident] $struct_name:ident, $( $x:ident ),* ) => {
        #[repr($repr)]
        pub struct $struct_name< $( $x ),* > ( $( <$x as WasmExternType>::Native ),* )
        where
            $( $x: WasmExternType ),*;

        impl< $( $x ),* > WasmTypeList for ( $( $x ),* )
        where
            $( $x: WasmExternType ),*
        {
            type CStruct = $struct_name<$( $x ),*>;

            type RetArray = [u64; count_idents!( $( $x ),* )];

            fn from_ret_array(array: Self::RetArray) -> Self {
                #[allow(non_snake_case)]
                let [ $( $x ),* ] = array;

                ( $( WasmExternType::from_native(NativeWasmType::from_binary($x)) ),* )
            }

            fn empty_ret_array() -> Self::RetArray {
                [0; count_idents!( $( $x ),* )]
            }

            fn from_c_struct(c_struct: Self::CStruct) -> Self {
                #[allow(non_snake_case)]
                let $struct_name ( $( $x ),* ) = c_struct;

                ( $( WasmExternType::from_native($x) ),* )
            }

            fn into_c_struct(self) -> Self::CStruct {
                #[allow(unused_parens, non_snake_case)]
                let ( $( $x ),* ) = self;

                $struct_name ( $( WasmExternType::to_native($x) ),* )
            }

            fn types() -> &'static [Type] {
                &[$( $x::Native::TYPE ),*]
            }

            #[allow(non_snake_case)]
            unsafe fn call<Rets>(
                self,
                f: NonNull<vm::Func>,
                wasm: Wasm,
                ctx: *mut vm::Ctx,
            ) -> Result<Rets, RuntimeError>
            where
                Rets: WasmTypeList
            {
                #[allow(unused_parens)]
                let ( $( $x ),* ) = self;
                let args = [ $( $x.to_native().to_binary()),* ];
                let mut rets = Rets::empty_ret_array();
                let mut trap = WasmTrapInfo::Unknown;
                let mut user_error = None;

                if (wasm.invoke)(
                    wasm.trampoline,
                    ctx,
                    f,
                    args.as_ptr(),
                    rets.as_mut().as_mut_ptr(),
                    &mut trap,
                    &mut user_error,
                    wasm.invoke_env
                ) {
                    Ok(Rets::from_ret_array(rets))
                } else {
                    if let Some(data) = user_error {
                        Err(RuntimeError::Error { data })
                    } else {
                        Err(RuntimeError::Trap { msg: trap.to_string().into() })
                    }
                }
            }
        }

        impl< $( $x, )* Rets, Trap, FN > ExternalFunction<ExplicitVmCtx, ( $( $x ),* ), Rets> for FN
        where
            $( $x: WasmExternType, )*
            Rets: WasmTypeList,
            Trap: TrapEarly<Rets>,
            FN: Fn(&mut vm::Ctx $( , $x )*) -> Trap,
        {
            #[allow(non_snake_case)]
            fn to_raw(&self) -> NonNull<vm::Func> {
                if mem::size_of::<Self>() == 0 {
                    /// This is required for the llvm backend to be able to unwind through this function.
                    #[cfg_attr(nightly, unwind(allowed))]
                    extern fn wrap<$( $x, )* Rets, Trap, FN>(
                        vmctx: &mut vm::Ctx $( , $x: <$x as WasmExternType>::Native )*
                    ) -> Rets::CStruct
                    where
                        $( $x: WasmExternType, )*
                        Rets: WasmTypeList,
                        Trap: TrapEarly<Rets>,
                        FN: Fn(&mut vm::Ctx, $( $x, )*) -> Trap,
                    {
                        let f: FN = unsafe { mem::transmute_copy(&()) };

                        let err = match panic::catch_unwind(
                            panic::AssertUnwindSafe(
                                || {
                                    f(vmctx $( , WasmExternType::from_native($x) )* ).report()
                                }
                            )
                        ) {
                            Ok(Ok(returns)) => return returns.into_c_struct(),
                            Ok(Err(err)) => {
                                let b: Box<_> = err.into();
                                b as Box<dyn Any>
                            },
                            Err(err) => err,
                        };

                        unsafe {
                            (&*vmctx.module).runnable_module.do_early_trap(err)
                        }
                    }

                    NonNull::new(wrap::<$( $x, )* Rets, Trap, Self> as *mut vm::Func).unwrap()
                } else {
                    assert_eq!(
                        mem::size_of::<Self>(),
                        mem::size_of::<usize>(),
                        "you cannot use a closure that captures state for `Func`."
                    );

                    NonNull::new(unsafe {
                        mem::transmute_copy::<_, *mut vm::Func>(self)
                    }).unwrap()
                }
            }
        }

        impl< $( $x, )* Rets, Trap, FN > ExternalFunction<ImplicitVmCtx, ( $( $x ),* ), Rets> for FN
        where
            $( $x: WasmExternType, )*
            Rets: WasmTypeList,
            Trap: TrapEarly<Rets>,
            FN: Fn($( $x, )*) -> Trap,
        {
            #[allow(non_snake_case)]
            fn to_raw(&self) -> NonNull<vm::Func> {
                if mem::size_of::<Self>() == 0 {
                    /// This is required for the llvm backend to be able to unwind through this function.
                    #[cfg_attr(nightly, unwind(allowed))]
                    extern fn wrap<$( $x, )* Rets, Trap, FN>(
                        vmctx: &mut vm::Ctx $( , $x: <$x as WasmExternType>::Native )*
                    ) -> Rets::CStruct
                    where
                        $( $x: WasmExternType, )*
                        Rets: WasmTypeList,
                        Trap: TrapEarly<Rets>,
                        FN: Fn($( $x, )*) -> Trap,
                    {
                        let f: FN = unsafe { mem::transmute_copy(&()) };

                        let err = match panic::catch_unwind(
                            panic::AssertUnwindSafe(
                                || {
                                    f($( WasmExternType::from_native($x), )* ).report()
                                }
                            )
                        ) {
                            Ok(Ok(returns)) => return returns.into_c_struct(),
                            Ok(Err(err)) => {
                                let b: Box<_> = err.into();
                                b as Box<dyn Any>
                            },
                            Err(err) => err,
                        };

                        unsafe {
                            (&*vmctx.module).runnable_module.do_early_trap(err)
                        }
                    }

                    NonNull::new(wrap::<$( $x, )* Rets, Trap, Self> as *mut vm::Func).unwrap()
                } else {
                    assert_eq!(
                        mem::size_of::<Self>(),
                        mem::size_of::<usize>(),
                        "you cannot use a closure that captures state for `Func`."
                    );

                    NonNull::new(unsafe {
                        mem::transmute_copy::<_, *mut vm::Func>(self)
                    }).unwrap()
                }
            }
        }

        impl<'a $( , $x )*, Rets> Func<'a, ( $( $x ),* ), Rets, Wasm>
        where
            $( $x: WasmExternType, )*
            Rets: WasmTypeList,
        {
            #[allow(non_snake_case)]
            pub fn call(&self, $( $x: $x, )* ) -> Result<Rets, RuntimeError> {
                #[allow(unused_parens)]
                unsafe {
                    <( $( $x ),* ) as WasmTypeList>::call(
                        ( $( $x ),* ),
                        self.f,
                        self.inner,
                        self.ctx
                    )
                }
            }
        }
    };
}

macro_rules! count_idents {
    ( $($idents:ident),* ) => {{
        #[allow(dead_code, non_camel_case_types)]
        enum Idents { $($idents,)* __CountIdentsLast }
        const COUNT: usize = Idents::__CountIdentsLast as usize;
        COUNT
    }};
}

impl_traits!([C] S0,);
impl_traits!([transparent] S1, A);
impl_traits!([C] S2, A, B);
impl_traits!([C] S3, A, B, C);
impl_traits!([C] S4, A, B, C, D);
impl_traits!([C] S5, A, B, C, D, E);
impl_traits!([C] S6, A, B, C, D, E, F);
impl_traits!([C] S7, A, B, C, D, E, F, G);
impl_traits!([C] S8, A, B, C, D, E, F, G, H);
impl_traits!([C] S9, A, B, C, D, E, F, G, H, I);
impl_traits!([C] S10, A, B, C, D, E, F, G, H, I, J);
impl_traits!([C] S11, A, B, C, D, E, F, G, H, I, J, K);
impl_traits!([C] S12, A, B, C, D, E, F, G, H, I, J, K, L);

impl<'a, Args, Rets, Inner> IsExport for Func<'a, Args, Rets, Inner>
where
    Args: WasmTypeList,
    Rets: WasmTypeList,
    Inner: Kind,
{
    fn to_export(&self) -> Export {
        let func = unsafe { FuncPointer::new(self.f.as_ptr()) };
        let ctx = Context::Internal;
        let signature = Arc::new(FuncSig::new(Args::types(), Rets::types()));

        Export::Function {
            func,
            ctx,
            signature,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! test_func_arity_n {
        ($test_name:ident, $($x:ident),*) => {
            #[test]
            fn $test_name() {
                use crate::vm;

                fn with_vmctx(_: &mut vm::Ctx, $($x: i32),*) -> i32 {
                    vec![$($x),*].iter().sum()
                }

                let _func = Func::new(with_vmctx);
            }
        }
    }

    #[test]
    fn test_func_arity_0() {
        fn foo(_: &mut vm::Ctx) -> i32 {
            0
        }

        let _ = Func::new(foo);
    }

    test_func_arity_n!(test_func_arity_1, a);
    test_func_arity_n!(test_func_arity_2, a, b);
    test_func_arity_n!(test_func_arity_3, a, b, c);
    test_func_arity_n!(test_func_arity_4, a, b, c, d);
    test_func_arity_n!(test_func_arity_5, a, b, c, d, e);
    test_func_arity_n!(test_func_arity_6, a, b, c, d, e, f);
    test_func_arity_n!(test_func_arity_7, a, b, c, d, e, f, g);
    test_func_arity_n!(test_func_arity_8, a, b, c, d, e, f, g, h);
    test_func_arity_n!(test_func_arity_9, a, b, c, d, e, f, g, h, i);
    test_func_arity_n!(test_func_arity_10, a, b, c, d, e, f, g, h, i, j);
    test_func_arity_n!(test_func_arity_11, a, b, c, d, e, f, g, h, i, j, k);
    test_func_arity_n!(test_func_arity_12, a, b, c, d, e, f, g, h, i, j, k, l);

    #[test]
    fn test_call() {
        fn foo(_ctx: &mut vm::Ctx, a: i32, b: i32) -> (i32, i32) {
            (a, b)
        }

        let _f = Func::new(foo);
    }

    #[test]
    fn test_imports() {
        use crate::{func, imports};

        fn foo(_ctx: &mut vm::Ctx, a: i32) -> i32 {
            a
        }

        let _import_object = imports! {
            "env" => {
                "foo" => func!(foo),
            },
        };
    }
}
