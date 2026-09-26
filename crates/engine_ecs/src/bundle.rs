//! Tuples of components spawned together.

use crate::component::{Component, ComponentId, Components};

/// Most components in one bundle.
pub const MAX_BUNDLE: usize = 12;

/// A tuple of up to [`MAX_BUNDLE`] distinct components, spawned together.
///
/// # Safety
///
/// `write` must initialize exactly one value at each destination, of the type whose id
/// `register` put at the same position.
pub unsafe trait Bundle: Send + Sync + 'static {
    /// Number of components.
    const COUNT: usize;

    /// Registers the component types and writes their ids in tuple order.
    fn register(components: &mut Components, out: &mut [ComponentId; MAX_BUNDLE]);

    /// Moves the components out.
    ///
    /// # Safety
    ///
    /// `dst[i]` points to uninitialized, aligned room for the tuple's `i`-th component.
    unsafe fn write(self, dst: &[*mut u8; MAX_BUNDLE]);
}

macro_rules! bundle {
    ($count:expr; $($t:ident $v:ident $i:tt),+) => {
        // SAFETY: writes each component to the destination of its tuple position.
        unsafe impl<$($t: Component),+> Bundle for ($($t,)+) {
            const COUNT: usize = $count;

            fn register(components: &mut Components, out: &mut [ComponentId; MAX_BUNDLE]) {
                $(out[$i] = components.register::<$t>();)+
            }

            #[inline]
            unsafe fn write(self, dst: &[*mut u8; MAX_BUNDLE]) {
                let ($($v,)+) = self;
                // SAFETY: guaranteed by the caller.
                unsafe { $(dst[$i].cast::<$t>().write($v);)+ }
            }
        }
    };
}

bundle!(1; A a 0);
bundle!(2; A a 0, B b 1);
bundle!(3; A a 0, B b 1, C c 2);
bundle!(4; A a 0, B b 1, C c 2, D d 3);
bundle!(5; A a 0, B b 1, C c 2, D d 3, E e 4);
bundle!(6; A a 0, B b 1, C c 2, D d 3, E e 4, F f 5);
bundle!(7; A a 0, B b 1, C c 2, D d 3, E e 4, F f 5, G g 6);
bundle!(8; A a 0, B b 1, C c 2, D d 3, E e 4, F f 5, G g 6, H h 7);
bundle!(9; A a 0, B b 1, C c 2, D d 3, E e 4, F f 5, G g 6, H h 7, I i 8);
bundle!(10; A a 0, B b 1, C c 2, D d 3, E e 4, F f 5, G g 6, H h 7, I i 8, J j 9);
bundle!(11; A a 0, B b 1, C c 2, D d 3, E e 4, F f 5, G g 6, H h 7, I i 8, J j 9, K k 10);
bundle!(12; A a 0, B b 1, C c 2, D d 3, E e 4, F f 5, G g 6, H h 7, I i 8, J j 9, K k 10, L l 11);
