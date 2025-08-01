use std::os::raw::c_void;
use std::{cmp, fmt::Debug, slice};

use arith::Field;
use itertools::izip;
use mpi::environment::Universe;
use mpi::{
    datatype::PartitionMut,
    ffi::*,
    topology::{Process, SimpleCommunicator},
    traits::*,
};
use serdes::ExpSerde;

use super::MPIEngine;

#[macro_export]
macro_rules! root_println {
    ($config: expr, $($arg:tt)*) => {
        if $config.is_root() {
            println!($($arg)*);
        }
    };
}

#[derive(Clone)]
pub struct MPIConfig<'a> {
    pub universe: Option<&'a Universe>,
    pub world: Option<&'a SimpleCommunicator>,
    pub world_size: i32,
    pub world_rank: i32,
}

impl<'a> Default for MPIConfig<'a> {
    fn default() -> Self {
        Self {
            universe: None,
            world: None,
            world_size: 1,
            world_rank: 0,
        }
    }
}

impl<'a> Debug for MPIConfig<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let universe_fmt = if self.universe.is_none() {
            Option::<usize>::None
        } else {
            Some(0usize)
        };

        let world_fmt = if self.world.is_none() {
            Option::<usize>::None
        } else {
            Some(0usize)
        };

        f.debug_struct("MPIConfig")
            .field("universe", &universe_fmt)
            .field("world", &world_fmt)
            .field("world_size", &self.world_size)
            .field("world_rank", &self.world_rank)
            .finish()
    }
}

// Note: may not be correct
impl<'a> PartialEq for MPIConfig<'a> {
    fn eq(&self, other: &Self) -> bool {
        self.world_rank == other.world_rank && self.world_size == other.world_size
    }
}

impl<'a> MPIConfig<'a> {
    /// The communication limit for MPI is 2^30. Save 10 bits for #parties here.
    pub const CHUNK_SIZE: usize = 1usize << 20;

    /// Initialize the MPI environment.
    /// Safe to call multiple times as `mpi::initialize()` will return None if already initialized.
    pub fn init() -> Option<Universe> {
        mpi::initialize()
    }

    /// Create a new MPI engine for the prover
    pub fn prover_new(
        universe: Option<&'a Universe>,
        communicator: Option<&'a SimpleCommunicator>,
    ) -> Self {
        let world = communicator;
        let (world_size, world_rank) = if let Some(world) = world {
            (world.size(), world.rank())
        } else {
            (1, 0)
        };
        Self {
            universe,
            world,
            world_size,
            world_rank,
        }
    }

    /// Create a new MPI engine for the verifier with specified world size
    ///
    /// # Arguments
    /// * `world_size` - The total number of processes in the MPI world
    #[inline]
    pub fn verifier_new(world_size: i32) -> Self {
        Self {
            universe: None,
            world: None,
            world_size,
            world_rank: 0,
        }
    }
}

/// MPI toolkit:
impl<'a> MPIEngine for MPIConfig<'a> {
    const ROOT_RANK: i32 = 0;

    #[allow(clippy::collapsible_else_if)]
    fn gather_vec<F: Sized + Clone>(&self, local_vec: &[F], global_vec: &mut Vec<F>) {
        unsafe {
            if self.world_size == 1 {
                *global_vec = local_vec.to_vec()
            } else {
                assert!(!self.is_root() || global_vec.len() == local_vec.len() * self.world_size());

                let local_vec_u8 = transmute_vec_to_u8_bytes(local_vec);
                let local_n_bytes = local_vec_u8.len();
                let n_chunks = (local_n_bytes + Self::CHUNK_SIZE - 1) / Self::CHUNK_SIZE;
                if n_chunks == 1 {
                    if self.world_rank == Self::ROOT_RANK {
                        let mut global_vec_u8 = transmute_vec_to_u8_bytes(global_vec);
                        self.root_process()
                            .gather_into_root(&local_vec_u8, &mut global_vec_u8);
                        global_vec_u8.leak(); // discard control of the memory
                    } else {
                        self.root_process().gather_into(&local_vec_u8);
                    }
                } else {
                    if self.world_rank == Self::ROOT_RANK {
                        let mut chunk_buffer_u8 = vec![0u8; Self::CHUNK_SIZE * self.world_size()];
                        let mut global_vec_u8 = transmute_vec_to_u8_bytes(global_vec);
                        for i in 0..n_chunks {
                            let local_start = i * Self::CHUNK_SIZE;
                            let local_end = cmp::min(local_start + Self::CHUNK_SIZE, local_n_bytes);
                            let actual_chunk_size = local_end - local_start;
                            if actual_chunk_size < Self::CHUNK_SIZE {
                                chunk_buffer_u8.resize(actual_chunk_size * self.world_size(), 0u8);
                            }

                            self.root_process().gather_into_root(
                                &local_vec_u8[local_start..local_end],
                                &mut chunk_buffer_u8,
                            );

                            // distribute the data to where they belong to in global vec
                            for j in 0..self.world_size() {
                                let global_start = j * local_n_bytes + local_start;
                                let global_end = global_start + actual_chunk_size;
                                global_vec_u8[global_start..global_end].copy_from_slice(
                                    &chunk_buffer_u8
                                        [j * actual_chunk_size..(j + 1) * actual_chunk_size],
                                );
                            }
                        }
                        global_vec_u8.leak(); // discard control of the memory
                    } else {
                        for i in 0..n_chunks {
                            let local_start = i * Self::CHUNK_SIZE;
                            let local_end = cmp::min(local_start + Self::CHUNK_SIZE, local_n_bytes);
                            self.root_process()
                                .gather_into(&local_vec_u8[local_start..local_end]);
                        }
                    }
                }
                local_vec_u8.leak(); // discard control of the memory
            }
        }
    }

    #[inline]
    fn scatter_vec<F: Sized + Clone>(&self, send_vec: &[F], recv_vec: &mut [F]) {
        if self.world_size() == 1 {
            recv_vec.clone_from_slice(send_vec);
            return;
        }

        let send_buf_u8_len = std::mem::size_of_val(send_vec);
        let send_u8s: &[u8] =
            unsafe { slice::from_raw_parts(send_vec.as_ptr() as *const u8, send_buf_u8_len) };

        let recv_buf_u8_len = std::mem::size_of_val(recv_vec);
        let recv_u8s: &mut [u8] =
            unsafe { slice::from_raw_parts_mut(recv_vec.as_mut_ptr() as *mut u8, recv_buf_u8_len) };

        let n_chunks = recv_buf_u8_len.div_ceil(Self::CHUNK_SIZE);

        if n_chunks == 1 {
            if self.is_root() {
                self.root_process().scatter_into_root(send_u8s, recv_u8s);
            } else {
                self.root_process().scatter_into(recv_u8s);
            }

            return;
        }

        if !self.is_root() {
            recv_u8s.chunks_mut(Self::CHUNK_SIZE).for_each(|c| {
                self.root_process().scatter_into(c);
            });

            return;
        }

        let mut send_buf = vec![0u8; Self::CHUNK_SIZE * self.world_size()];

        izip!(0..n_chunks, recv_u8s.chunks_mut(Self::CHUNK_SIZE)).for_each(|(i, recv_c)| {
            let copy_srt = i * Self::CHUNK_SIZE;
            let copy_end = copy_srt + recv_c.len();

            if recv_c.len() < Self::CHUNK_SIZE {
                send_buf.resize(recv_c.len() * self.world_size(), 0u8);
            }

            izip!(0..self.world_size(), send_buf.chunks_mut(recv_c.len())).for_each(
                |(world_i, send_c)| {
                    let world_starts = recv_buf_u8_len * world_i;

                    let local_srt = world_starts + copy_srt;
                    let local_end = world_starts + copy_end;

                    send_c.copy_from_slice(&send_u8s[local_srt..local_end]);
                },
            );

            self.root_process().scatter_into_root(&send_buf, recv_c);
        })
    }

    /// Root process broadcast a value f into all the processes
    #[inline]
    fn root_broadcast_f<F: Copy>(&self, f: &mut F) {
        unsafe {
            if self.world_size == 1 {
            } else {
                let mut vec_u8 = transmute_elem_to_u8_bytes(f, std::mem::size_of::<F>());
                self.root_process().broadcast_into(&mut vec_u8);
                vec_u8.leak();
            }
        }
    }

    #[inline]
    fn root_broadcast_bytes(&self, bytes: &mut Vec<u8>) {
        if self.world_size == 1 {
            return;
        }
        self.root_process().broadcast_into(bytes);
    }

    /// sum up all local values
    #[inline]
    fn sum_vec<F: Field>(&self, local_vec: &[F]) -> Vec<F> {
        if self.world_size == 1 {
            local_vec.to_vec()
        } else if self.world_rank == Self::ROOT_RANK {
            let mut global_vec = vec![F::ZERO; local_vec.len() * (self.world_size as usize)];
            self.gather_vec(local_vec, &mut global_vec);
            for i in 0..local_vec.len() {
                for j in 1..(self.world_size as usize) {
                    global_vec[i] = global_vec[i] + global_vec[j * local_vec.len() + i];
                }
            }
            global_vec.truncate(local_vec.len());
            global_vec
        } else {
            self.gather_vec(local_vec, &mut vec![]);
            vec![]
        }
    }

    /// coef has a length of mpi_world_size
    #[inline]
    fn coef_combine_vec<F: Field>(&self, local_vec: &[F], coef: &[F]) -> Vec<F> {
        if self.world_size == 1 {
            // Warning: literally, it should be coef[0] * local_vec
            // but coef[0] is always one in our use case of self.world_size = 1
            local_vec.to_vec()
        } else if self.world_rank == Self::ROOT_RANK {
            let mut global_vec = vec![F::ZERO; local_vec.len() * (self.world_size as usize)];
            let mut ret = vec![F::ZERO; local_vec.len()];
            self.gather_vec(local_vec, &mut global_vec);
            for i in 0..local_vec.len() {
                for j in 0..(self.world_size as usize) {
                    ret[i] += global_vec[j * local_vec.len() + i] * coef[j];
                }
            }
            ret
        } else {
            self.gather_vec(local_vec, &mut vec![]);
            vec![F::ZERO; local_vec.len()]
        }
    }

    /// perform an all to all transpose,
    /// supposing the current party holds a row in a matrix with row number being MPI parties.
    #[inline(always)]
    fn all_to_all_transpose<F: Sized>(&self, row: &mut [F]) {
        assert_eq!(row.len() % self.world_size(), 0);

        // NOTE(HS) MPI has some upper limit for send buffer size, pre declare here and use later
        const SEND_BUFFER_MAX: usize = 1 << 22;

        let row_as_u8_len = size_of_val(row);
        let row_u8s: &mut [u8] =
            unsafe { slice::from_raw_parts_mut(row.as_mut_ptr() as *mut u8, row_as_u8_len) };

        let num_of_bytes_per_world = row_as_u8_len / self.world_size();
        let num_of_transposes = row_as_u8_len.div_ceil(SEND_BUFFER_MAX);

        let mut send = vec![0u8; SEND_BUFFER_MAX];
        let mut recv = vec![0u8; SEND_BUFFER_MAX];

        let mut send_buffer_size = SEND_BUFFER_MAX;
        let mut copy_starts = 0;

        (0..num_of_transposes).for_each(|ith_transpose| {
            if ith_transpose == num_of_transposes - 1 {
                send_buffer_size = (num_of_bytes_per_world - copy_starts) * self.world_size();
                send.resize(send_buffer_size, 0u8);
                recv.resize(send_buffer_size, 0u8);
            }

            let send_buffer_size_per_world = send_buffer_size / self.world_size();
            let copy_ends = copy_starts + send_buffer_size_per_world;

            izip!(
                row_u8s.chunks(num_of_bytes_per_world),
                send.chunks_mut(send_buffer_size_per_world)
            )
            .for_each(|(row_chunk, send_chunk)| {
                send_chunk.copy_from_slice(&row_chunk[copy_starts..copy_ends]);
            });

            self.world.unwrap().all_to_all_into(&send, &mut recv);

            izip!(
                row_u8s.chunks_mut(num_of_bytes_per_world),
                recv.chunks(send_buffer_size_per_world)
            )
            .for_each(|(row_chunk, recv_chunk)| {
                row_chunk[copy_starts..copy_ends].copy_from_slice(recv_chunk);
            });

            copy_starts += send_buffer_size_per_world;
        });
    }

    #[inline(always)]
    fn gather_varlen_vec<F: ExpSerde>(&self, elems: &Vec<F>, global_elems: &mut Vec<Vec<F>>) {
        let mut elems_bytes: Vec<u8> = Vec::new();
        elems.serialize_into(&mut elems_bytes).unwrap();

        let mut byte_lengths = vec![0i32; self.world_size()];
        self.gather_vec(&[elems_bytes.len() as i32], &mut byte_lengths);

        let all_elems_bytes_len = byte_lengths.iter().sum::<i32>() as usize;
        let mut all_elems_bytes: Vec<u8> = vec![0u8; all_elems_bytes_len];

        if !self.is_root() {
            self.root_process().gather_varcount_into(&elems_bytes);
        } else {
            let displs = byte_lengths
                .iter()
                .scan(0, |s, i| {
                    let srt = *s;
                    *s += i;
                    Some(srt)
                })
                .collect::<Vec<_>>();

            let mut partition = PartitionMut::new(&mut all_elems_bytes, byte_lengths, &displs[..]);

            self.root_process()
                .gather_varcount_into_root(&elems_bytes, &mut partition);

            *global_elems = displs
                .iter()
                .map(|&srt| Vec::deserialize_from(&all_elems_bytes[srt as usize..]).unwrap())
                .collect();
        }
    }

    #[inline(always)]
    fn is_single_process(&self) -> bool {
        self.world_size == 1
    }

    #[inline(always)]
    fn world_size(&self) -> usize {
        self.world_size as usize
    }

    #[inline(always)]
    fn world_rank(&self) -> usize {
        self.world_rank as usize
    }

    #[inline(always)]
    fn root_process(&self) -> Process {
        self.world.unwrap().process_at_rank(Self::ROOT_RANK)
    }

    // Barrier is designed for mpi use only
    // There might be some issues if used with multi-threading
    #[inline(always)]
    fn barrier(&self) {
        if self.world_size > 1 {
            self.world.unwrap().barrier();
        }
    }

    #[inline]
    fn create_shared_mem(&self, n_bytes: usize) -> (*mut u8, *mut ompi_win_t) {
        let window_size = if self.is_root() { n_bytes } else { 0 };
        let mut baseptr: *mut c_void = std::ptr::null_mut();

        // Handle to the MPI window.  Initialize to null and let MPI fill it in.
        let mut window_handle: MPI_Win = MPI_Win(std::ptr::null_mut());

        unsafe {
            MPI_Win_allocate_shared(
                window_size as isize,
                1,
                RSMPI_INFO_NULL,
                self.world.unwrap().as_raw(),
                &mut baseptr as *mut *mut c_void as *mut c_void,
                &mut window_handle as *mut MPI_Win,
            );
            self.barrier();

            if !self.is_root() {
                let mut size: MPI_Aint = 0;
                let mut disp_unit: ::std::os::raw::c_int = 0;
                let mut query_baseptr: *mut c_void = std::ptr::null_mut();
                MPI_Win_shared_query(
                    window_handle,
                    0,
                    &mut size as *mut MPI_Aint,
                    &mut disp_unit as *mut ::std::os::raw::c_int,
                    &mut query_baseptr as *mut *mut c_void as *mut c_void,
                );
                baseptr = query_baseptr;
            }
        }

        (baseptr as *mut u8, window_handle.0)
    }
}

/// Return an u8 vector sharing THE SAME MEMORY SLOT with the input.
#[inline]
unsafe fn transmute_elem_to_u8_bytes<V: Sized>(elem: &V, byte_size: usize) -> Vec<u8> {
    Vec::<u8>::from_raw_parts((elem as *const V) as *mut u8, byte_size, byte_size)
}

/// Return an u8 vector sharing THE SAME MEMORY SLOT with the input.
#[inline]
unsafe fn transmute_vec_to_u8_bytes<F: Sized>(vec: &[F]) -> Vec<u8> {
    Vec::<u8>::from_raw_parts(
        vec.as_ptr() as *mut u8,
        std::mem::size_of_val(vec),
        std::mem::size_of_val(vec),
    )
}
