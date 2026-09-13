#[macro_export] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:931
#[doc(hidden)] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:932
macro_rules! isle_numerics_methods {
    () => {
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a == b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_ne( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a != b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_lt( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a < b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_lt_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a <= b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_gt( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a > b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_gt_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a >= b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_checked_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i8> {
            a.checked_add(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_wrapping_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i8 {
            a.wrapping_add(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i8 {
            a.checked_add(b).unwrap_or_else(|| panic!("addition overflow: {a} + {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_checked_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i8> {
            a.checked_sub(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_wrapping_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i8 {
            a.wrapping_sub(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i8 {
            a.checked_sub(b).unwrap_or_else(|| panic!("subtraction overflow: {a} - {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_checked_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i8> {
            a.checked_mul(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_wrapping_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i8 {
            a.wrapping_mul(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i8 {
            a.checked_mul(b).unwrap_or_else(|| panic!("multiplication overflow: {a} * {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_checked_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i8> {
            a.checked_div(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_wrapping_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i8 {
            a.wrapping_div(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i8 {
            a.checked_div(b).unwrap_or_else(|| panic!("div failure: {a} / {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_checked_rem( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i8> {
            a.checked_rem(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_rem( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i8 {
            a.checked_rem(b).unwrap_or_else(|| panic!("rem failure: {a} % {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_and( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i8 {
            a & b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_or( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i8 {
            a | b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_xor( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i8 {
            a ^ b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_not( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i8 {
            !a // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_checked_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i8> {
            a.checked_shl(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_wrapping_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i8 {
            a.wrapping_shl(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i8 {
            a.checked_shl(b).unwrap_or_else(|| panic!("shl overflow: {a} << {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_checked_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i8> {
            a.checked_shr(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_wrapping_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i8 {
            a.wrapping_shr(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i8 {
            a.checked_shr(b).unwrap_or_else(|| panic!("shr overflow: {a} >> {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_rotl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i8 {
            a.rotate_left(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_rotr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i8 {
            a.rotate_right(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_is_zero( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a == 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn i8_matches_zero(&mut self, a: i8) -> Option<bool> {
            Some(a == 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_is_non_zero( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a != 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn i8_matches_non_zero(&mut self, a: i8) -> Option<bool> {
            Some(a != 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_is_odd( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a & 1 == 1 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn i8_matches_odd(&mut self, a: i8) -> Option<bool> {
            Some(a & 1 == 1) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_is_even( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a & 1 == 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn i8_matches_even(&mut self, a: i8) -> Option<bool> {
            Some(a & 1 == 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_checked_ilog2( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u32> {
            a.checked_ilog2() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_ilog2( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.checked_ilog2().unwrap_or_else(|| panic!("ilog2 overflow: {a}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_trailing_zeros( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.trailing_zeros() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_trailing_ones( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.trailing_ones() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_leading_zeros( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.leading_zeros() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_leading_ones( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.leading_ones() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_checked_neg( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i8> {
            a.checked_neg() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_wrapping_neg( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i8 {
            a.wrapping_neg() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i8_neg( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i8 {
            a.checked_neg().unwrap_or_else(|| panic!("negation overflow: {a}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a == b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_ne( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a != b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_lt( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a < b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_lt_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a <= b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_gt( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a > b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_gt_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a >= b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_checked_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u8> {
            a.checked_add(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_wrapping_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u8 {
            a.wrapping_add(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u8 {
            a.checked_add(b).unwrap_or_else(|| panic!("addition overflow: {a} + {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_checked_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u8> {
            a.checked_sub(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_wrapping_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u8 {
            a.wrapping_sub(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u8 {
            a.checked_sub(b).unwrap_or_else(|| panic!("subtraction overflow: {a} - {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_checked_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u8> {
            a.checked_mul(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_wrapping_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u8 {
            a.wrapping_mul(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u8 {
            a.checked_mul(b).unwrap_or_else(|| panic!("multiplication overflow: {a} * {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_checked_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u8> {
            a.checked_div(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_wrapping_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u8 {
            a.wrapping_div(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u8 {
            a.checked_div(b).unwrap_or_else(|| panic!("div failure: {a} / {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_checked_rem( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u8> {
            a.checked_rem(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_rem( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u8 {
            a.checked_rem(b).unwrap_or_else(|| panic!("rem failure: {a} % {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_and( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u8 {
            a & b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_or( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u8 {
            a | b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_xor( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u8 {
            a ^ b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_not( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u8 {
            !a // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_checked_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u8> {
            a.checked_shl(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_wrapping_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u8 {
            a.wrapping_shl(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u8 {
            a.checked_shl(b).unwrap_or_else(|| panic!("shl overflow: {a} << {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_checked_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u8> {
            a.checked_shr(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_wrapping_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u8 {
            a.wrapping_shr(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u8 {
            a.checked_shr(b).unwrap_or_else(|| panic!("shr overflow: {a} >> {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_rotl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u8 {
            a.rotate_left(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_rotr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u8 {
            a.rotate_right(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_is_zero( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a == 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u8_matches_zero(&mut self, a: u8) -> Option<bool> {
            Some(a == 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_is_non_zero( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a != 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u8_matches_non_zero(&mut self, a: u8) -> Option<bool> {
            Some(a != 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_is_odd( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a & 1 == 1 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u8_matches_odd(&mut self, a: u8) -> Option<bool> {
            Some(a & 1 == 1) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_is_even( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a & 1 == 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u8_matches_even(&mut self, a: u8) -> Option<bool> {
            Some(a & 1 == 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_checked_ilog2( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u32> {
            a.checked_ilog2() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_ilog2( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.checked_ilog2().unwrap_or_else(|| panic!("ilog2 overflow: {a}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_trailing_zeros( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.trailing_zeros() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_trailing_ones( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.trailing_ones() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_leading_zeros( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.leading_zeros() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_leading_ones( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.leading_ones() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u8_is_power_of_two( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a.is_power_of_two() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u8_matches_power_of_two(&mut self, a: u8) -> Option<bool> {
            Some(a.is_power_of_two()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a == b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_ne( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a != b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_lt( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a < b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_lt_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a <= b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_gt( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a > b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_gt_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a >= b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_checked_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i16> {
            a.checked_add(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_wrapping_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i16 {
            a.wrapping_add(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i16 {
            a.checked_add(b).unwrap_or_else(|| panic!("addition overflow: {a} + {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_checked_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i16> {
            a.checked_sub(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_wrapping_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i16 {
            a.wrapping_sub(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i16 {
            a.checked_sub(b).unwrap_or_else(|| panic!("subtraction overflow: {a} - {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_checked_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i16> {
            a.checked_mul(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_wrapping_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i16 {
            a.wrapping_mul(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i16 {
            a.checked_mul(b).unwrap_or_else(|| panic!("multiplication overflow: {a} * {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_checked_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i16> {
            a.checked_div(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_wrapping_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i16 {
            a.wrapping_div(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i16 {
            a.checked_div(b).unwrap_or_else(|| panic!("div failure: {a} / {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_checked_rem( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i16> {
            a.checked_rem(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_rem( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i16 {
            a.checked_rem(b).unwrap_or_else(|| panic!("rem failure: {a} % {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_and( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i16 {
            a & b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_or( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i16 {
            a | b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_xor( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i16 {
            a ^ b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_not( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i16 {
            !a // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_checked_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i16> {
            a.checked_shl(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_wrapping_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i16 {
            a.wrapping_shl(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i16 {
            a.checked_shl(b).unwrap_or_else(|| panic!("shl overflow: {a} << {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_checked_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i16> {
            a.checked_shr(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_wrapping_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i16 {
            a.wrapping_shr(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i16 {
            a.checked_shr(b).unwrap_or_else(|| panic!("shr overflow: {a} >> {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_rotl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i16 {
            a.rotate_left(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_rotr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i16 {
            a.rotate_right(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_is_zero( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a == 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn i16_matches_zero(&mut self, a: i16) -> Option<bool> {
            Some(a == 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_is_non_zero( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a != 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn i16_matches_non_zero(&mut self, a: i16) -> Option<bool> {
            Some(a != 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_is_odd( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a & 1 == 1 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn i16_matches_odd(&mut self, a: i16) -> Option<bool> {
            Some(a & 1 == 1) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_is_even( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a & 1 == 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn i16_matches_even(&mut self, a: i16) -> Option<bool> {
            Some(a & 1 == 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_checked_ilog2( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u32> {
            a.checked_ilog2() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_ilog2( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.checked_ilog2().unwrap_or_else(|| panic!("ilog2 overflow: {a}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_trailing_zeros( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.trailing_zeros() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_trailing_ones( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.trailing_ones() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_leading_zeros( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.leading_zeros() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_leading_ones( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.leading_ones() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_checked_neg( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i16> {
            a.checked_neg() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_wrapping_neg( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i16 {
            a.wrapping_neg() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i16_neg( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i16 {
            a.checked_neg().unwrap_or_else(|| panic!("negation overflow: {a}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a == b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_ne( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a != b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_lt( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a < b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_lt_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a <= b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_gt( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a > b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_gt_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a >= b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_checked_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u16> {
            a.checked_add(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_wrapping_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u16 {
            a.wrapping_add(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u16 {
            a.checked_add(b).unwrap_or_else(|| panic!("addition overflow: {a} + {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_checked_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u16> {
            a.checked_sub(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_wrapping_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u16 {
            a.wrapping_sub(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u16 {
            a.checked_sub(b).unwrap_or_else(|| panic!("subtraction overflow: {a} - {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_checked_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u16> {
            a.checked_mul(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_wrapping_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u16 {
            a.wrapping_mul(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u16 {
            a.checked_mul(b).unwrap_or_else(|| panic!("multiplication overflow: {a} * {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_checked_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u16> {
            a.checked_div(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_wrapping_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u16 {
            a.wrapping_div(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u16 {
            a.checked_div(b).unwrap_or_else(|| panic!("div failure: {a} / {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_checked_rem( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u16> {
            a.checked_rem(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_rem( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u16 {
            a.checked_rem(b).unwrap_or_else(|| panic!("rem failure: {a} % {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_and( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u16 {
            a & b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_or( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u16 {
            a | b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_xor( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u16 {
            a ^ b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_not( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u16 {
            !a // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_checked_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u16> {
            a.checked_shl(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_wrapping_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u16 {
            a.wrapping_shl(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u16 {
            a.checked_shl(b).unwrap_or_else(|| panic!("shl overflow: {a} << {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_checked_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u16> {
            a.checked_shr(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_wrapping_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u16 {
            a.wrapping_shr(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u16 {
            a.checked_shr(b).unwrap_or_else(|| panic!("shr overflow: {a} >> {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_rotl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u16 {
            a.rotate_left(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_rotr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u16 {
            a.rotate_right(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_is_zero( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a == 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u16_matches_zero(&mut self, a: u16) -> Option<bool> {
            Some(a == 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_is_non_zero( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a != 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u16_matches_non_zero(&mut self, a: u16) -> Option<bool> {
            Some(a != 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_is_odd( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a & 1 == 1 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u16_matches_odd(&mut self, a: u16) -> Option<bool> {
            Some(a & 1 == 1) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_is_even( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a & 1 == 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u16_matches_even(&mut self, a: u16) -> Option<bool> {
            Some(a & 1 == 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_checked_ilog2( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u32> {
            a.checked_ilog2() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_ilog2( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.checked_ilog2().unwrap_or_else(|| panic!("ilog2 overflow: {a}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_trailing_zeros( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.trailing_zeros() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_trailing_ones( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.trailing_ones() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_leading_zeros( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.leading_zeros() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_leading_ones( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.leading_ones() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u16_is_power_of_two( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a.is_power_of_two() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u16_matches_power_of_two(&mut self, a: u16) -> Option<bool> {
            Some(a.is_power_of_two()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a == b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_ne( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a != b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_lt( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a < b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_lt_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a <= b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_gt( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a > b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_gt_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a >= b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_checked_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i32> {
            a.checked_add(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_wrapping_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i32 {
            a.wrapping_add(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i32 {
            a.checked_add(b).unwrap_or_else(|| panic!("addition overflow: {a} + {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_checked_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i32> {
            a.checked_sub(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_wrapping_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i32 {
            a.wrapping_sub(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i32 {
            a.checked_sub(b).unwrap_or_else(|| panic!("subtraction overflow: {a} - {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_checked_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i32> {
            a.checked_mul(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_wrapping_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i32 {
            a.wrapping_mul(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i32 {
            a.checked_mul(b).unwrap_or_else(|| panic!("multiplication overflow: {a} * {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_checked_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i32> {
            a.checked_div(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_wrapping_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i32 {
            a.wrapping_div(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i32 {
            a.checked_div(b).unwrap_or_else(|| panic!("div failure: {a} / {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_checked_rem( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i32> {
            a.checked_rem(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_rem( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i32 {
            a.checked_rem(b).unwrap_or_else(|| panic!("rem failure: {a} % {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_and( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i32 {
            a & b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_or( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i32 {
            a | b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_xor( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i32 {
            a ^ b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_not( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i32 {
            !a // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_checked_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i32> {
            a.checked_shl(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_wrapping_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i32 {
            a.wrapping_shl(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i32 {
            a.checked_shl(b).unwrap_or_else(|| panic!("shl overflow: {a} << {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_checked_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i32> {
            a.checked_shr(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_wrapping_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i32 {
            a.wrapping_shr(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i32 {
            a.checked_shr(b).unwrap_or_else(|| panic!("shr overflow: {a} >> {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_rotl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i32 {
            a.rotate_left(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_rotr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i32 {
            a.rotate_right(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_is_zero( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a == 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn i32_matches_zero(&mut self, a: i32) -> Option<bool> {
            Some(a == 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_is_non_zero( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a != 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn i32_matches_non_zero(&mut self, a: i32) -> Option<bool> {
            Some(a != 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_is_odd( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a & 1 == 1 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn i32_matches_odd(&mut self, a: i32) -> Option<bool> {
            Some(a & 1 == 1) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_is_even( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a & 1 == 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn i32_matches_even(&mut self, a: i32) -> Option<bool> {
            Some(a & 1 == 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_checked_ilog2( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u32> {
            a.checked_ilog2() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_ilog2( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.checked_ilog2().unwrap_or_else(|| panic!("ilog2 overflow: {a}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_trailing_zeros( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.trailing_zeros() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_trailing_ones( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.trailing_ones() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_leading_zeros( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.leading_zeros() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_leading_ones( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.leading_ones() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_checked_neg( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i32> {
            a.checked_neg() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_wrapping_neg( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i32 {
            a.wrapping_neg() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i32_neg( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i32 {
            a.checked_neg().unwrap_or_else(|| panic!("negation overflow: {a}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a == b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_ne( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a != b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_lt( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a < b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_lt_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a <= b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_gt( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a > b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_gt_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a >= b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_checked_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u32> {
            a.checked_add(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_wrapping_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.wrapping_add(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.checked_add(b).unwrap_or_else(|| panic!("addition overflow: {a} + {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_checked_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u32> {
            a.checked_sub(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_wrapping_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.wrapping_sub(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.checked_sub(b).unwrap_or_else(|| panic!("subtraction overflow: {a} - {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_checked_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u32> {
            a.checked_mul(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_wrapping_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.wrapping_mul(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.checked_mul(b).unwrap_or_else(|| panic!("multiplication overflow: {a} * {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_checked_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u32> {
            a.checked_div(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_wrapping_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.wrapping_div(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.checked_div(b).unwrap_or_else(|| panic!("div failure: {a} / {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_checked_rem( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u32> {
            a.checked_rem(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_rem( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.checked_rem(b).unwrap_or_else(|| panic!("rem failure: {a} % {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_and( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a & b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_or( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a | b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_xor( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a ^ b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_not( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            !a // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_checked_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u32> {
            a.checked_shl(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_wrapping_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.wrapping_shl(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.checked_shl(b).unwrap_or_else(|| panic!("shl overflow: {a} << {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_checked_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u32> {
            a.checked_shr(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_wrapping_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.wrapping_shr(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.checked_shr(b).unwrap_or_else(|| panic!("shr overflow: {a} >> {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_rotl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.rotate_left(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_rotr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.rotate_right(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_is_zero( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a == 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u32_matches_zero(&mut self, a: u32) -> Option<bool> {
            Some(a == 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_is_non_zero( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a != 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u32_matches_non_zero(&mut self, a: u32) -> Option<bool> {
            Some(a != 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_is_odd( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a & 1 == 1 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u32_matches_odd(&mut self, a: u32) -> Option<bool> {
            Some(a & 1 == 1) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_is_even( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a & 1 == 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u32_matches_even(&mut self, a: u32) -> Option<bool> {
            Some(a & 1 == 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_checked_ilog2( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u32> {
            a.checked_ilog2() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_ilog2( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.checked_ilog2().unwrap_or_else(|| panic!("ilog2 overflow: {a}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_trailing_zeros( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.trailing_zeros() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_trailing_ones( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.trailing_ones() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_leading_zeros( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.leading_zeros() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_leading_ones( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.leading_ones() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u32_is_power_of_two( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a.is_power_of_two() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u32_matches_power_of_two(&mut self, a: u32) -> Option<bool> {
            Some(a.is_power_of_two()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a == b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_ne( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a != b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_lt( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a < b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_lt_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a <= b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_gt( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a > b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_gt_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a >= b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_checked_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i64> {
            a.checked_add(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_wrapping_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i64 {
            a.wrapping_add(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i64 {
            a.checked_add(b).unwrap_or_else(|| panic!("addition overflow: {a} + {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_checked_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i64> {
            a.checked_sub(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_wrapping_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i64 {
            a.wrapping_sub(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i64 {
            a.checked_sub(b).unwrap_or_else(|| panic!("subtraction overflow: {a} - {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_checked_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i64> {
            a.checked_mul(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_wrapping_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i64 {
            a.wrapping_mul(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i64 {
            a.checked_mul(b).unwrap_or_else(|| panic!("multiplication overflow: {a} * {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_checked_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i64> {
            a.checked_div(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_wrapping_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i64 {
            a.wrapping_div(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i64 {
            a.checked_div(b).unwrap_or_else(|| panic!("div failure: {a} / {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_checked_rem( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i64> {
            a.checked_rem(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_rem( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i64 {
            a.checked_rem(b).unwrap_or_else(|| panic!("rem failure: {a} % {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_and( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i64 {
            a & b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_or( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i64 {
            a | b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_xor( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i64 {
            a ^ b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_not( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i64 {
            !a // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_checked_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i64> {
            a.checked_shl(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_wrapping_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i64 {
            a.wrapping_shl(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i64 {
            a.checked_shl(b).unwrap_or_else(|| panic!("shl overflow: {a} << {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_checked_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i64> {
            a.checked_shr(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_wrapping_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i64 {
            a.wrapping_shr(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i64 {
            a.checked_shr(b).unwrap_or_else(|| panic!("shr overflow: {a} >> {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_rotl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i64 {
            a.rotate_left(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_rotr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i64 {
            a.rotate_right(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_is_zero( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a == 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn i64_matches_zero(&mut self, a: i64) -> Option<bool> {
            Some(a == 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_is_non_zero( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a != 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn i64_matches_non_zero(&mut self, a: i64) -> Option<bool> {
            Some(a != 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_is_odd( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a & 1 == 1 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn i64_matches_odd(&mut self, a: i64) -> Option<bool> {
            Some(a & 1 == 1) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_is_even( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a & 1 == 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn i64_matches_even(&mut self, a: i64) -> Option<bool> {
            Some(a & 1 == 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_checked_ilog2( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u32> {
            a.checked_ilog2() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_ilog2( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.checked_ilog2().unwrap_or_else(|| panic!("ilog2 overflow: {a}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_trailing_zeros( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.trailing_zeros() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_trailing_ones( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.trailing_ones() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_leading_zeros( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.leading_zeros() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_leading_ones( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.leading_ones() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_checked_neg( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i64> {
            a.checked_neg() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_wrapping_neg( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i64 {
            a.wrapping_neg() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i64_neg( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i64 {
            a.checked_neg().unwrap_or_else(|| panic!("negation overflow: {a}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a == b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_ne( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a != b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_lt( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a < b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_lt_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a <= b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_gt( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a > b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_gt_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a >= b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_checked_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u64> {
            a.checked_add(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_wrapping_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u64 {
            a.wrapping_add(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u64 {
            a.checked_add(b).unwrap_or_else(|| panic!("addition overflow: {a} + {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_checked_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u64> {
            a.checked_sub(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_wrapping_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u64 {
            a.wrapping_sub(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u64 {
            a.checked_sub(b).unwrap_or_else(|| panic!("subtraction overflow: {a} - {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_checked_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u64> {
            a.checked_mul(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_wrapping_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u64 {
            a.wrapping_mul(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u64 {
            a.checked_mul(b).unwrap_or_else(|| panic!("multiplication overflow: {a} * {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_checked_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u64> {
            a.checked_div(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_wrapping_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u64 {
            a.wrapping_div(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u64 {
            a.checked_div(b).unwrap_or_else(|| panic!("div failure: {a} / {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_checked_rem( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u64> {
            a.checked_rem(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_rem( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u64 {
            a.checked_rem(b).unwrap_or_else(|| panic!("rem failure: {a} % {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_and( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u64 {
            a & b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_or( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u64 {
            a | b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_xor( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u64 {
            a ^ b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_not( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u64 {
            !a // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_checked_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u64> {
            a.checked_shl(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_wrapping_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u64 {
            a.wrapping_shl(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u64 {
            a.checked_shl(b).unwrap_or_else(|| panic!("shl overflow: {a} << {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_checked_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u64> {
            a.checked_shr(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_wrapping_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u64 {
            a.wrapping_shr(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u64 {
            a.checked_shr(b).unwrap_or_else(|| panic!("shr overflow: {a} >> {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_rotl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u64 {
            a.rotate_left(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_rotr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u64 {
            a.rotate_right(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_is_zero( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a == 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u64_matches_zero(&mut self, a: u64) -> Option<bool> {
            Some(a == 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_is_non_zero( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a != 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u64_matches_non_zero(&mut self, a: u64) -> Option<bool> {
            Some(a != 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_is_odd( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a & 1 == 1 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u64_matches_odd(&mut self, a: u64) -> Option<bool> {
            Some(a & 1 == 1) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_is_even( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a & 1 == 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u64_matches_even(&mut self, a: u64) -> Option<bool> {
            Some(a & 1 == 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_checked_ilog2( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u32> {
            a.checked_ilog2() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_ilog2( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.checked_ilog2().unwrap_or_else(|| panic!("ilog2 overflow: {a}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_trailing_zeros( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.trailing_zeros() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_trailing_ones( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.trailing_ones() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_leading_zeros( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.leading_zeros() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_leading_ones( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.leading_ones() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u64_is_power_of_two( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a.is_power_of_two() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u64_matches_power_of_two(&mut self, a: u64) -> Option<bool> {
            Some(a.is_power_of_two()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a == b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_ne( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a != b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_lt( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a < b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_lt_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a <= b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_gt( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a > b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_gt_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a >= b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_checked_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i128> {
            a.checked_add(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_wrapping_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i128 {
            a.wrapping_add(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i128 {
            a.checked_add(b).unwrap_or_else(|| panic!("addition overflow: {a} + {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_checked_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i128> {
            a.checked_sub(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_wrapping_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i128 {
            a.wrapping_sub(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i128 {
            a.checked_sub(b).unwrap_or_else(|| panic!("subtraction overflow: {a} - {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_checked_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i128> {
            a.checked_mul(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_wrapping_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i128 {
            a.wrapping_mul(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i128 {
            a.checked_mul(b).unwrap_or_else(|| panic!("multiplication overflow: {a} * {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_checked_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i128> {
            a.checked_div(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_wrapping_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i128 {
            a.wrapping_div(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i128 {
            a.checked_div(b).unwrap_or_else(|| panic!("div failure: {a} / {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_checked_rem( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i128> {
            a.checked_rem(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_rem( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i128 {
            a.checked_rem(b).unwrap_or_else(|| panic!("rem failure: {a} % {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_and( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i128 {
            a & b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_or( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i128 {
            a | b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_xor( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i128 {
            a ^ b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_not( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i128 {
            !a // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_checked_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i128> {
            a.checked_shl(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_wrapping_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i128 {
            a.wrapping_shl(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i128 {
            a.checked_shl(b).unwrap_or_else(|| panic!("shl overflow: {a} << {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_checked_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i128> {
            a.checked_shr(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_wrapping_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i128 {
            a.wrapping_shr(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i128 {
            a.checked_shr(b).unwrap_or_else(|| panic!("shr overflow: {a} >> {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_rotl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i128 {
            a.rotate_left(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_rotr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i128 {
            a.rotate_right(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_is_zero( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a == 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn i128_matches_zero(&mut self, a: i128) -> Option<bool> {
            Some(a == 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_is_non_zero( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a != 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn i128_matches_non_zero(&mut self, a: i128) -> Option<bool> {
            Some(a != 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_is_odd( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a & 1 == 1 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn i128_matches_odd(&mut self, a: i128) -> Option<bool> {
            Some(a & 1 == 1) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_is_even( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a & 1 == 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn i128_matches_even(&mut self, a: i128) -> Option<bool> {
            Some(a & 1 == 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_checked_ilog2( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u32> {
            a.checked_ilog2() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_ilog2( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.checked_ilog2().unwrap_or_else(|| panic!("ilog2 overflow: {a}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_trailing_zeros( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.trailing_zeros() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_trailing_ones( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.trailing_ones() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_leading_zeros( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.leading_zeros() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_leading_ones( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.leading_ones() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_checked_neg( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<i128> {
            a.checked_neg() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_wrapping_neg( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i128 {
            a.wrapping_neg() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn i128_neg( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: i128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> i128 {
            a.checked_neg().unwrap_or_else(|| panic!("negation overflow: {a}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a == b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_ne( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a != b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_lt( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a < b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_lt_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a <= b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_gt( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a > b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_gt_eq( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a >= b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_checked_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u128> {
            a.checked_add(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_wrapping_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u128 {
            a.wrapping_add(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_add( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u128 {
            a.checked_add(b).unwrap_or_else(|| panic!("addition overflow: {a} + {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_checked_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u128> {
            a.checked_sub(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_wrapping_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u128 {
            a.wrapping_sub(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_sub( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u128 {
            a.checked_sub(b).unwrap_or_else(|| panic!("subtraction overflow: {a} - {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_checked_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u128> {
            a.checked_mul(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_wrapping_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u128 {
            a.wrapping_mul(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_mul( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u128 {
            a.checked_mul(b).unwrap_or_else(|| panic!("multiplication overflow: {a} * {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_checked_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u128> {
            a.checked_div(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_wrapping_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u128 {
            a.wrapping_div(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_div( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u128 {
            a.checked_div(b).unwrap_or_else(|| panic!("div failure: {a} / {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_checked_rem( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u128> {
            a.checked_rem(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_rem( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u128 {
            a.checked_rem(b).unwrap_or_else(|| panic!("rem failure: {a} % {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_and( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u128 {
            a & b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_or( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u128 {
            a | b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_xor( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u128 {
            a ^ b // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_not( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u128 {
            !a // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_checked_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u128> {
            a.checked_shl(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_wrapping_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u128 {
            a.wrapping_shl(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_shl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u128 {
            a.checked_shl(b).unwrap_or_else(|| panic!("shl overflow: {a} << {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_checked_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u128> {
            a.checked_shr(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_wrapping_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u128 {
            a.wrapping_shr(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_shr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u128 {
            a.checked_shr(b).unwrap_or_else(|| panic!("shr overflow: {a} >> {b}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_rotl( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u128 {
            a.rotate_left(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_rotr( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
            b: u32, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u128 {
            a.rotate_right(b) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_is_zero( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a == 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u128_matches_zero(&mut self, a: u128) -> Option<bool> {
            Some(a == 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_is_non_zero( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a != 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u128_matches_non_zero(&mut self, a: u128) -> Option<bool> {
            Some(a != 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_is_odd( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a & 1 == 1 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u128_matches_odd(&mut self, a: u128) -> Option<bool> {
            Some(a & 1 == 1) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_is_even( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a & 1 == 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u128_matches_even(&mut self, a: u128) -> Option<bool> {
            Some(a & 1 == 0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_checked_ilog2( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> Option<u32> {
            a.checked_ilog2() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_ilog2( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.checked_ilog2().unwrap_or_else(|| panic!("ilog2 overflow: {a}")) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_trailing_zeros( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.trailing_zeros() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_trailing_ones( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.trailing_ones() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_leading_zeros( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.leading_zeros() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_leading_ones( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> u32 {
            a.leading_ones() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:959
        fn u128_is_power_of_two( // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:960
            &mut self, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:962
            a: u128, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:964
        ) -> bool {
            a.is_power_of_two() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:969
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:999
        fn u128_matches_power_of_two(&mut self, a: u128) -> Option<bool> {
            Some(a.is_power_of_two()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1005
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i8_try_into_u8(&mut self, x: i8) -> Option<u8> {
            u8::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i8_unwrap_into_u8(&mut self, x: i8) -> u8 {
            u8::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1175
        fn i8_cast_unsigned(&mut self, x: i8) -> u8 {
            x as u8 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1183
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i8_from_u8(&mut self, x: i8) -> Option<u8> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i8_into_i16(&mut self, x: i8) -> i16 {
            i16::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i8_from_i16(&mut self, x: i8) -> Option<i16> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i8_try_into_u16(&mut self, x: i8) -> Option<u16> {
            u16::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i8_unwrap_into_u16(&mut self, x: i8) -> u16 {
            u16::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i8_from_u16(&mut self, x: i8) -> Option<u16> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i8_into_i32(&mut self, x: i8) -> i32 {
            i32::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i8_from_i32(&mut self, x: i8) -> Option<i32> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i8_try_into_u32(&mut self, x: i8) -> Option<u32> {
            u32::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i8_unwrap_into_u32(&mut self, x: i8) -> u32 {
            u32::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i8_from_u32(&mut self, x: i8) -> Option<u32> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i8_into_i64(&mut self, x: i8) -> i64 {
            i64::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i8_from_i64(&mut self, x: i8) -> Option<i64> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i8_try_into_u64(&mut self, x: i8) -> Option<u64> {
            u64::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i8_unwrap_into_u64(&mut self, x: i8) -> u64 {
            u64::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i8_from_u64(&mut self, x: i8) -> Option<u64> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i8_into_i128(&mut self, x: i8) -> i128 {
            i128::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i8_from_i128(&mut self, x: i8) -> Option<i128> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i8_try_into_u128(&mut self, x: i8) -> Option<u128> {
            u128::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i8_unwrap_into_u128(&mut self, x: i8) -> u128 {
            u128::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i8_from_u128(&mut self, x: i8) -> Option<u128> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u8_try_into_i8(&mut self, x: u8) -> Option<i8> {
            i8::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u8_unwrap_into_i8(&mut self, x: u8) -> i8 {
            i8::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1175
        fn u8_cast_signed(&mut self, x: u8) -> i8 {
            x as i8 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1183
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u8_from_i8(&mut self, x: u8) -> Option<i8> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u8_into_i16(&mut self, x: u8) -> i16 {
            i16::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u8_from_i16(&mut self, x: u8) -> Option<i16> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u8_into_u16(&mut self, x: u8) -> u16 {
            u16::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u8_from_u16(&mut self, x: u8) -> Option<u16> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u8_into_i32(&mut self, x: u8) -> i32 {
            i32::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u8_from_i32(&mut self, x: u8) -> Option<i32> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u8_into_u32(&mut self, x: u8) -> u32 {
            u32::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u8_from_u32(&mut self, x: u8) -> Option<u32> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u8_into_i64(&mut self, x: u8) -> i64 {
            i64::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u8_from_i64(&mut self, x: u8) -> Option<i64> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u8_into_u64(&mut self, x: u8) -> u64 {
            u64::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u8_from_u64(&mut self, x: u8) -> Option<u64> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u8_into_i128(&mut self, x: u8) -> i128 {
            i128::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u8_from_i128(&mut self, x: u8) -> Option<i128> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u8_into_u128(&mut self, x: u8) -> u128 {
            u128::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u8_from_u128(&mut self, x: u8) -> Option<u128> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i16_try_into_i8(&mut self, x: i16) -> Option<i8> {
            i8::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i16_unwrap_into_i8(&mut self, x: i16) -> i8 {
            i8::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1148
        fn i16_truncate_into_i8(&mut self, x: i16) -> i8 {
            x as i8 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1154
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i16_from_i8(&mut self, x: i16) -> Option<i8> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i16_try_into_u8(&mut self, x: i16) -> Option<u8> {
            u8::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i16_unwrap_into_u8(&mut self, x: i16) -> u8 {
            u8::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i16_from_u8(&mut self, x: i16) -> Option<u8> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i16_try_into_u16(&mut self, x: i16) -> Option<u16> {
            u16::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i16_unwrap_into_u16(&mut self, x: i16) -> u16 {
            u16::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1175
        fn i16_cast_unsigned(&mut self, x: i16) -> u16 {
            x as u16 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1183
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i16_from_u16(&mut self, x: i16) -> Option<u16> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i16_into_i32(&mut self, x: i16) -> i32 {
            i32::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i16_from_i32(&mut self, x: i16) -> Option<i32> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i16_try_into_u32(&mut self, x: i16) -> Option<u32> {
            u32::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i16_unwrap_into_u32(&mut self, x: i16) -> u32 {
            u32::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i16_from_u32(&mut self, x: i16) -> Option<u32> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i16_into_i64(&mut self, x: i16) -> i64 {
            i64::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i16_from_i64(&mut self, x: i16) -> Option<i64> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i16_try_into_u64(&mut self, x: i16) -> Option<u64> {
            u64::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i16_unwrap_into_u64(&mut self, x: i16) -> u64 {
            u64::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i16_from_u64(&mut self, x: i16) -> Option<u64> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i16_into_i128(&mut self, x: i16) -> i128 {
            i128::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i16_from_i128(&mut self, x: i16) -> Option<i128> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i16_try_into_u128(&mut self, x: i16) -> Option<u128> {
            u128::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i16_unwrap_into_u128(&mut self, x: i16) -> u128 {
            u128::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i16_from_u128(&mut self, x: i16) -> Option<u128> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u16_try_into_i8(&mut self, x: u16) -> Option<i8> {
            i8::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u16_unwrap_into_i8(&mut self, x: u16) -> i8 {
            i8::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u16_from_i8(&mut self, x: u16) -> Option<i8> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u16_try_into_u8(&mut self, x: u16) -> Option<u8> {
            u8::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u16_unwrap_into_u8(&mut self, x: u16) -> u8 {
            u8::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1148
        fn u16_truncate_into_u8(&mut self, x: u16) -> u8 {
            x as u8 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1154
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u16_from_u8(&mut self, x: u16) -> Option<u8> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u16_try_into_i16(&mut self, x: u16) -> Option<i16> {
            i16::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u16_unwrap_into_i16(&mut self, x: u16) -> i16 {
            i16::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1175
        fn u16_cast_signed(&mut self, x: u16) -> i16 {
            x as i16 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1183
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u16_from_i16(&mut self, x: u16) -> Option<i16> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u16_into_i32(&mut self, x: u16) -> i32 {
            i32::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u16_from_i32(&mut self, x: u16) -> Option<i32> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u16_into_u32(&mut self, x: u16) -> u32 {
            u32::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u16_from_u32(&mut self, x: u16) -> Option<u32> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u16_into_i64(&mut self, x: u16) -> i64 {
            i64::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u16_from_i64(&mut self, x: u16) -> Option<i64> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u16_into_u64(&mut self, x: u16) -> u64 {
            u64::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u16_from_u64(&mut self, x: u16) -> Option<u64> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u16_into_i128(&mut self, x: u16) -> i128 {
            i128::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u16_from_i128(&mut self, x: u16) -> Option<i128> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u16_into_u128(&mut self, x: u16) -> u128 {
            u128::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u16_from_u128(&mut self, x: u16) -> Option<u128> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i32_try_into_i8(&mut self, x: i32) -> Option<i8> {
            i8::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i32_unwrap_into_i8(&mut self, x: i32) -> i8 {
            i8::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1148
        fn i32_truncate_into_i8(&mut self, x: i32) -> i8 {
            x as i8 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1154
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i32_from_i8(&mut self, x: i32) -> Option<i8> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i32_try_into_u8(&mut self, x: i32) -> Option<u8> {
            u8::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i32_unwrap_into_u8(&mut self, x: i32) -> u8 {
            u8::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i32_from_u8(&mut self, x: i32) -> Option<u8> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i32_try_into_i16(&mut self, x: i32) -> Option<i16> {
            i16::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i32_unwrap_into_i16(&mut self, x: i32) -> i16 {
            i16::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1148
        fn i32_truncate_into_i16(&mut self, x: i32) -> i16 {
            x as i16 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1154
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i32_from_i16(&mut self, x: i32) -> Option<i16> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i32_try_into_u16(&mut self, x: i32) -> Option<u16> {
            u16::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i32_unwrap_into_u16(&mut self, x: i32) -> u16 {
            u16::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i32_from_u16(&mut self, x: i32) -> Option<u16> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i32_try_into_u32(&mut self, x: i32) -> Option<u32> {
            u32::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i32_unwrap_into_u32(&mut self, x: i32) -> u32 {
            u32::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1175
        fn i32_cast_unsigned(&mut self, x: i32) -> u32 {
            x as u32 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1183
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i32_from_u32(&mut self, x: i32) -> Option<u32> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i32_into_i64(&mut self, x: i32) -> i64 {
            i64::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i32_from_i64(&mut self, x: i32) -> Option<i64> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i32_try_into_u64(&mut self, x: i32) -> Option<u64> {
            u64::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i32_unwrap_into_u64(&mut self, x: i32) -> u64 {
            u64::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i32_from_u64(&mut self, x: i32) -> Option<u64> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i32_into_i128(&mut self, x: i32) -> i128 {
            i128::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i32_from_i128(&mut self, x: i32) -> Option<i128> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i32_try_into_u128(&mut self, x: i32) -> Option<u128> {
            u128::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i32_unwrap_into_u128(&mut self, x: i32) -> u128 {
            u128::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i32_from_u128(&mut self, x: i32) -> Option<u128> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u32_try_into_i8(&mut self, x: u32) -> Option<i8> {
            i8::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u32_unwrap_into_i8(&mut self, x: u32) -> i8 {
            i8::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u32_from_i8(&mut self, x: u32) -> Option<i8> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u32_try_into_u8(&mut self, x: u32) -> Option<u8> {
            u8::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u32_unwrap_into_u8(&mut self, x: u32) -> u8 {
            u8::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1148
        fn u32_truncate_into_u8(&mut self, x: u32) -> u8 {
            x as u8 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1154
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u32_from_u8(&mut self, x: u32) -> Option<u8> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u32_try_into_i16(&mut self, x: u32) -> Option<i16> {
            i16::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u32_unwrap_into_i16(&mut self, x: u32) -> i16 {
            i16::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u32_from_i16(&mut self, x: u32) -> Option<i16> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u32_try_into_u16(&mut self, x: u32) -> Option<u16> {
            u16::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u32_unwrap_into_u16(&mut self, x: u32) -> u16 {
            u16::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1148
        fn u32_truncate_into_u16(&mut self, x: u32) -> u16 {
            x as u16 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1154
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u32_from_u16(&mut self, x: u32) -> Option<u16> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u32_try_into_i32(&mut self, x: u32) -> Option<i32> {
            i32::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u32_unwrap_into_i32(&mut self, x: u32) -> i32 {
            i32::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1175
        fn u32_cast_signed(&mut self, x: u32) -> i32 {
            x as i32 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1183
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u32_from_i32(&mut self, x: u32) -> Option<i32> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u32_into_i64(&mut self, x: u32) -> i64 {
            i64::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u32_from_i64(&mut self, x: u32) -> Option<i64> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u32_into_u64(&mut self, x: u32) -> u64 {
            u64::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u32_from_u64(&mut self, x: u32) -> Option<u64> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u32_into_i128(&mut self, x: u32) -> i128 {
            i128::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u32_from_i128(&mut self, x: u32) -> Option<i128> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u32_into_u128(&mut self, x: u32) -> u128 {
            u128::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u32_from_u128(&mut self, x: u32) -> Option<u128> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i64_try_into_i8(&mut self, x: i64) -> Option<i8> {
            i8::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i64_unwrap_into_i8(&mut self, x: i64) -> i8 {
            i8::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1148
        fn i64_truncate_into_i8(&mut self, x: i64) -> i8 {
            x as i8 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1154
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i64_from_i8(&mut self, x: i64) -> Option<i8> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i64_try_into_u8(&mut self, x: i64) -> Option<u8> {
            u8::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i64_unwrap_into_u8(&mut self, x: i64) -> u8 {
            u8::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i64_from_u8(&mut self, x: i64) -> Option<u8> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i64_try_into_i16(&mut self, x: i64) -> Option<i16> {
            i16::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i64_unwrap_into_i16(&mut self, x: i64) -> i16 {
            i16::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1148
        fn i64_truncate_into_i16(&mut self, x: i64) -> i16 {
            x as i16 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1154
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i64_from_i16(&mut self, x: i64) -> Option<i16> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i64_try_into_u16(&mut self, x: i64) -> Option<u16> {
            u16::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i64_unwrap_into_u16(&mut self, x: i64) -> u16 {
            u16::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i64_from_u16(&mut self, x: i64) -> Option<u16> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i64_try_into_i32(&mut self, x: i64) -> Option<i32> {
            i32::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i64_unwrap_into_i32(&mut self, x: i64) -> i32 {
            i32::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1148
        fn i64_truncate_into_i32(&mut self, x: i64) -> i32 {
            x as i32 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1154
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i64_from_i32(&mut self, x: i64) -> Option<i32> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i64_try_into_u32(&mut self, x: i64) -> Option<u32> {
            u32::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i64_unwrap_into_u32(&mut self, x: i64) -> u32 {
            u32::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i64_from_u32(&mut self, x: i64) -> Option<u32> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i64_try_into_u64(&mut self, x: i64) -> Option<u64> {
            u64::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i64_unwrap_into_u64(&mut self, x: i64) -> u64 {
            u64::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1175
        fn i64_cast_unsigned(&mut self, x: i64) -> u64 {
            x as u64 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1183
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i64_from_u64(&mut self, x: i64) -> Option<u64> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i64_into_i128(&mut self, x: i64) -> i128 {
            i128::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i64_from_i128(&mut self, x: i64) -> Option<i128> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i64_try_into_u128(&mut self, x: i64) -> Option<u128> {
            u128::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i64_unwrap_into_u128(&mut self, x: i64) -> u128 {
            u128::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i64_from_u128(&mut self, x: i64) -> Option<u128> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u64_try_into_i8(&mut self, x: u64) -> Option<i8> {
            i8::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u64_unwrap_into_i8(&mut self, x: u64) -> i8 {
            i8::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u64_from_i8(&mut self, x: u64) -> Option<i8> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u64_try_into_u8(&mut self, x: u64) -> Option<u8> {
            u8::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u64_unwrap_into_u8(&mut self, x: u64) -> u8 {
            u8::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1148
        fn u64_truncate_into_u8(&mut self, x: u64) -> u8 {
            x as u8 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1154
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u64_from_u8(&mut self, x: u64) -> Option<u8> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u64_try_into_i16(&mut self, x: u64) -> Option<i16> {
            i16::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u64_unwrap_into_i16(&mut self, x: u64) -> i16 {
            i16::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u64_from_i16(&mut self, x: u64) -> Option<i16> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u64_try_into_u16(&mut self, x: u64) -> Option<u16> {
            u16::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u64_unwrap_into_u16(&mut self, x: u64) -> u16 {
            u16::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1148
        fn u64_truncate_into_u16(&mut self, x: u64) -> u16 {
            x as u16 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1154
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u64_from_u16(&mut self, x: u64) -> Option<u16> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u64_try_into_i32(&mut self, x: u64) -> Option<i32> {
            i32::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u64_unwrap_into_i32(&mut self, x: u64) -> i32 {
            i32::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u64_from_i32(&mut self, x: u64) -> Option<i32> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u64_try_into_u32(&mut self, x: u64) -> Option<u32> {
            u32::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u64_unwrap_into_u32(&mut self, x: u64) -> u32 {
            u32::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1148
        fn u64_truncate_into_u32(&mut self, x: u64) -> u32 {
            x as u32 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1154
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u64_from_u32(&mut self, x: u64) -> Option<u32> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u64_try_into_i64(&mut self, x: u64) -> Option<i64> {
            i64::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u64_unwrap_into_i64(&mut self, x: u64) -> i64 {
            i64::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1175
        fn u64_cast_signed(&mut self, x: u64) -> i64 {
            x as i64 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1183
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u64_from_i64(&mut self, x: u64) -> Option<i64> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u64_into_i128(&mut self, x: u64) -> i128 {
            i128::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u64_from_i128(&mut self, x: u64) -> Option<i128> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u64_into_u128(&mut self, x: u64) -> u128 {
            u128::from(x) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1112
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u64_from_u128(&mut self, x: u64) -> Option<u128> {
            Some(x.into()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1206
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i128_try_into_i8(&mut self, x: i128) -> Option<i8> {
            i8::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i128_unwrap_into_i8(&mut self, x: i128) -> i8 {
            i8::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1148
        fn i128_truncate_into_i8(&mut self, x: i128) -> i8 {
            x as i8 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1154
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i128_from_i8(&mut self, x: i128) -> Option<i8> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i128_try_into_u8(&mut self, x: i128) -> Option<u8> {
            u8::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i128_unwrap_into_u8(&mut self, x: i128) -> u8 {
            u8::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i128_from_u8(&mut self, x: i128) -> Option<u8> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i128_try_into_i16(&mut self, x: i128) -> Option<i16> {
            i16::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i128_unwrap_into_i16(&mut self, x: i128) -> i16 {
            i16::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1148
        fn i128_truncate_into_i16(&mut self, x: i128) -> i16 {
            x as i16 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1154
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i128_from_i16(&mut self, x: i128) -> Option<i16> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i128_try_into_u16(&mut self, x: i128) -> Option<u16> {
            u16::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i128_unwrap_into_u16(&mut self, x: i128) -> u16 {
            u16::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i128_from_u16(&mut self, x: i128) -> Option<u16> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i128_try_into_i32(&mut self, x: i128) -> Option<i32> {
            i32::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i128_unwrap_into_i32(&mut self, x: i128) -> i32 {
            i32::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1148
        fn i128_truncate_into_i32(&mut self, x: i128) -> i32 {
            x as i32 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1154
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i128_from_i32(&mut self, x: i128) -> Option<i32> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i128_try_into_u32(&mut self, x: i128) -> Option<u32> {
            u32::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i128_unwrap_into_u32(&mut self, x: i128) -> u32 {
            u32::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i128_from_u32(&mut self, x: i128) -> Option<u32> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i128_try_into_i64(&mut self, x: i128) -> Option<i64> {
            i64::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i128_unwrap_into_i64(&mut self, x: i128) -> i64 {
            i64::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1148
        fn i128_truncate_into_i64(&mut self, x: i128) -> i64 {
            x as i64 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1154
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i128_from_i64(&mut self, x: i128) -> Option<i64> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i128_try_into_u64(&mut self, x: i128) -> Option<u64> {
            u64::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i128_unwrap_into_u64(&mut self, x: i128) -> u64 {
            u64::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i128_from_u64(&mut self, x: i128) -> Option<u64> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn i128_try_into_u128(&mut self, x: i128) -> Option<u128> {
            u128::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn i128_unwrap_into_u128(&mut self, x: i128) -> u128 {
            u128::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1175
        fn i128_cast_unsigned(&mut self, x: i128) -> u128 {
            x as u128 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1183
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn i128_from_u128(&mut self, x: i128) -> Option<u128> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u128_try_into_i8(&mut self, x: u128) -> Option<i8> {
            i8::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u128_unwrap_into_i8(&mut self, x: u128) -> i8 {
            i8::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u128_from_i8(&mut self, x: u128) -> Option<i8> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u128_try_into_u8(&mut self, x: u128) -> Option<u8> {
            u8::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u128_unwrap_into_u8(&mut self, x: u128) -> u8 {
            u8::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1148
        fn u128_truncate_into_u8(&mut self, x: u128) -> u8 {
            x as u8 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1154
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u128_from_u8(&mut self, x: u128) -> Option<u8> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u128_try_into_i16(&mut self, x: u128) -> Option<i16> {
            i16::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u128_unwrap_into_i16(&mut self, x: u128) -> i16 {
            i16::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u128_from_i16(&mut self, x: u128) -> Option<i16> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u128_try_into_u16(&mut self, x: u128) -> Option<u16> {
            u16::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u128_unwrap_into_u16(&mut self, x: u128) -> u16 {
            u16::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1148
        fn u128_truncate_into_u16(&mut self, x: u128) -> u16 {
            x as u16 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1154
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u128_from_u16(&mut self, x: u128) -> Option<u16> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u128_try_into_i32(&mut self, x: u128) -> Option<i32> {
            i32::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u128_unwrap_into_i32(&mut self, x: u128) -> i32 {
            i32::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u128_from_i32(&mut self, x: u128) -> Option<i32> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u128_try_into_u32(&mut self, x: u128) -> Option<u32> {
            u32::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u128_unwrap_into_u32(&mut self, x: u128) -> u32 {
            u32::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1148
        fn u128_truncate_into_u32(&mut self, x: u128) -> u32 {
            x as u32 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1154
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u128_from_u32(&mut self, x: u128) -> Option<u32> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u128_try_into_i64(&mut self, x: u128) -> Option<i64> {
            i64::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u128_unwrap_into_i64(&mut self, x: u128) -> i64 {
            i64::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u128_from_i64(&mut self, x: u128) -> Option<i64> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u128_try_into_u64(&mut self, x: u128) -> Option<u64> {
            u64::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u128_unwrap_into_u64(&mut self, x: u128) -> u64 {
            u64::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1148
        fn u128_truncate_into_u64(&mut self, x: u128) -> u64 {
            x as u64 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1154
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u128_from_u64(&mut self, x: u128) -> Option<u64> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1103
        fn u128_try_into_i128(&mut self, x: u128) -> Option<i128> {
            i128::try_from(x).ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1110
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1127
        fn u128_unwrap_into_i128(&mut self, x: u128) -> i128 {
            i128::try_from(x).unwrap() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1133
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1175
        fn u128_cast_signed(&mut self, x: u128) -> i128 {
            x as i128 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1183
        }
        #[inline] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1197
        fn u128_from_i128(&mut self, x: u128) -> Option<i128> {
            x.try_into().ok() // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_isle.rs:1204
        }

    }
}
