use super::instruction_hooking::{get_hooked_offsets, hook_instruction, unhook_instruction};
use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::{cell::RefCell, error::Error};
use std::{
	collections::HashMap,
	net::{SocketAddr, TcpListener, TcpStream},
	thread::JoinHandle,
};

use clap::{App, AppSettings, Arg};

use super::server_types::*;
use auxtools::raw_types::values::{ValueData, ValueTag};
use auxtools::*;

#[derive(Clone, Hash, Eq, PartialEq)]
enum Variables {
	Arguments {
		frame: u32,
	},
	Locals {
		frame: u32,
	},
	ObjectVars {
		tag: u8,
		data: u32,
	},
	ListContents {
		tag: u8,
		data: u32,
	},
	ListPair {
		key_tag: u8,
		key_data: u32,
		value_tag: u8,
		value_data: u32,
	},
	Internals {
		frame: u32,
	},
	Stack {
		frame: u32,
	},
}

struct State {
	stacks: debug::CallStacks,
	variables: RefCell<Vec<Variables>>,
	variables_to_refs: RefCell<HashMap<Variables, VariablesRef>>,
}

impl State {
	fn new() -> Self {
		Self {
			stacks: debug::CallStacks::new(&DMContext {}),
			variables: RefCell::new(vec![]),
			variables_to_refs: RefCell::new(HashMap::new()),
		}
	}

	fn get_ref(&self, vars: Variables) -> VariablesRef {
		let mut variables_to_refs = self.variables_to_refs.borrow_mut();
		let mut variables = self.variables.borrow_mut();
		(*variables_to_refs.entry(vars.clone()).or_insert_with(|| {
			let reference = VariablesRef(variables.len() as i32 + 1);
			variables.push(vars);
			reference
		}))
		.clone()
	}

	fn get_variables(&self, reference: VariablesRef) -> Option<Variables> {
		let variables = self.variables.borrow();
		variables
			.get(reference.0 as usize - 1)
			.map(|x| (*x).clone())
	}
}

//
// Server = main-thread code
// ServerThread = networking-thread code
//
// We've got a couple of channels going on between Server/ServerThread
// connection: a TcpStream sent from the ServerThread for the Server to send responses on
// requests: requests from the debug-client for the Server to handle
//
// Limitations: only ever accepts one connection
//

enum ServerStream {
	// The server is waiting for a Stream to be sent on the connection channel
	Waiting(mpsc::Receiver<TcpStream>),

	Connected(TcpStream),

	// The server has finished being used
	Disconnected,
}

pub struct Server {
	requests: mpsc::Receiver<Request>,
	stream: ServerStream,
	_thread: JoinHandle<()>,
	should_catch_runtimes: bool,
	should_show_internals: bool,
	app: App<'static, 'static>,
}

struct ServerThread {
	requests: mpsc::Sender<Request>,
}

impl Server {
	pub fn setup_app() -> App<'static, 'static> {
		App::new("Auxtools Debug Server")
			.version(clap::crate_version!())
			.settings(&[
				AppSettings::SubcommandRequired,
			])
			.global_settings(&[
				AppSettings::NoBinaryName,
				AppSettings::ColorNever,
				AppSettings::DisableVersion,
				AppSettings::VersionlessSubcommands,
				AppSettings::DisableHelpFlags,
			])
			.usage("#<SUBCOMMAND>")
			.subcommand(
				App::new("disassemble")
					.alias("dis")
					.about("Disassembles a proc and displays its bytecode in an assembly-like format")
					.after_help("If no parameters are provided, the proc executing in the currently debugged stack frame will be disassembled")
					.arg(
						Arg::with_name("proc")
							.help("Path of the proc to disassemble (e.g. /proc/do_stuff)")
							.takes_value(true),
					)
					.arg(
						Arg::with_name("id")
							.help("Id of the proc to disassemble (for when multiple procs are defined with the same path)")
							.takes_value(true),
					)
		)
	}

	pub fn connect(addr: &SocketAddr) -> std::io::Result<Server> {
		let stream = TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(5))?;
		let (requests_sender, requests_receiver) = mpsc::channel();

		let server_thread = ServerThread {
			requests: requests_sender,
		};

		let cloned_stream = stream.try_clone().unwrap();
		let thread = thread::spawn(move || {
			server_thread.run(cloned_stream);
		});

		let mut server = Server {
			requests: requests_receiver,
			stream: ServerStream::Connected(stream),
			_thread: thread,
			should_catch_runtimes: true,
			should_show_internals: true,
			app: Self::setup_app(),
		};

		server.process_until_configured();
		return Ok(server);
	}

	pub fn listen(addr: &SocketAddr) -> std::io::Result<Server> {
		let (connection_sender, connection_receiver) = mpsc::channel();
		let (requests_sender, requests_receiver) = mpsc::channel();

		let thread = ServerThread {
			requests: requests_sender,
		}
		.spawn_listener(TcpListener::bind(addr)?, connection_sender);

		Ok(Server {
			requests: requests_receiver,
			stream: ServerStream::Waiting(connection_receiver),
			_thread: thread,
			should_catch_runtimes: true,
			should_show_internals: true,
			app: Self::setup_app(),
		})
	}

	fn get_line_number(&self, proc: ProcRef, offset: u32) -> Option<u32> {
		match auxtools::Proc::find_override(proc.path, proc.override_id) {
			Some(proc) => {
				// We're ignoring disassemble errors because any bytecode in the result is still valid
				// stepping over unknown bytecode still works, but trying to set breakpoints in it can fail
				let dism = proc.disassemble(None).instructions;
				let mut current_line_number = None;
				let mut reached_offset = false;

				for (instruction_offset, _, instruction) in dism {
					// If we're in the middle of executing an operand (like call), the offset might be between two instructions
					if instruction_offset > offset {
						reached_offset = true;
						break;
					}

					if let Instruction::DbgLine(line) = instruction {
						current_line_number = Some(line);
					}

					if instruction_offset == offset {
						reached_offset = true;
						break;
					}
				}

				if reached_offset {
					return current_line_number;
				} else {
					return None;
				}
			}

			None => None,
		}
	}

	fn get_offset(&self, proc: ProcRef, line: u32) -> Option<u32> {
		match auxtools::Proc::find_override(proc.path, proc.override_id) {
			Some(proc) => {
				// We're ignoring disassemble errors because any bytecode in the result is still valid
				// stepping over unknown bytecode still works, but trying to set breakpoints in it can fail
				let dism = proc.disassemble(None).instructions;
				let mut offset = None;
				let mut at_offset = false;

				for (instruction_offset, _, instruction) in dism {
					if at_offset {
						offset = Some(instruction_offset);
						break;
					}
					if let Instruction::DbgLine(current_line) = instruction {
						if current_line == line {
							at_offset = true;
						}
					}
				}

				return offset;
			}

			None => {
				return None;
			}
		}
	}

	fn is_object(value: &Value) -> bool {
		// Hack for globals
		if value.value.tag == ValueTag::World && unsafe { value.value.data.id == 1 } {
			return true;
		}

		value.get("vars").is_ok()
	}

	fn value_to_variable(&self, state: &State, name: String, value: &Value) -> Variable {
		let mut variables = None;

		if List::is_list(value) {
			variables = Some(state.get_ref(Variables::ListContents {
				tag: value.value.tag as u8,
				data: unsafe { value.value.data.id },
			}));

			// Early return for lists so we can include their length in the value
			let stringified = match List::from_value(value) {
				Ok(list) => format!("/list {{len = {}}}", list.len()),
				Err(Runtime { message }) => format!("/list (failed to get len: {:?})", message),
			};

			return Variable {
				name,
				value: stringified,
				variables,
			};
		} else if Self::is_object(value) {
			variables = Some(state.get_ref(Variables::ObjectVars {
				tag: value.value.tag as u8,
				data: unsafe { value.value.data.id },
			}));
		}

		let stringified = match value.to_string() {
			Ok(v) if v.is_empty() => value.value.to_string(),
			Ok(value) => value,
			Err(Runtime { message }) => {
				format!("{} -- stringify error: {:?}", value.value, message)
			}
		};

		Variable {
			name,
			value: stringified,
			variables,
		}
	}

	fn list_to_variables(
		&mut self,
		state: &State,
		value: &Value,
	) -> Result<Vec<Variable>, Runtime> {
		let list = List::from_value(value)?;
		let len = list.len();

		let mut variables = vec![];

		for i in 1..=len {
			let key = list.get(i)?;

			if let Ok(value) = list.get(&key) {
				if value.value.tag != raw_types::values::ValueTag::Null {
					// assoc entry
					variables.push(Variable {
						name: format!("[{}]", i),
						value: format!("{} = {}", key.to_string()?, value.to_string()?), // TODO: prettify these prints?
						variables: Some(state.get_ref(unsafe {
							Variables::ListPair {
								key_tag: key.value.tag as u8,
								key_data: key.value.data.id,
								value_tag: value.value.tag as u8,
								value_data: value.value.data.id,
							}
						})),
					});
					continue;
				}
			}

			// non-assoc entry
			variables.push(self.value_to_variable(state, format!("[{}]", i), &key));
		}

		return Ok(variables);
	}

	fn object_to_variables(
		&mut self,
		state: &State,
		value: &Value,
	) -> Result<Vec<Variable>, Runtime> {
		// Grab `value.vars`. We have a little hack for globals which use a special type.
		let vars = List::from_value(&unsafe {
			if value.value.tag == ValueTag::World && value.value.data.id == 1 {
				Value::new(ValueTag::GlobalVars, ValueData { id: 0 })
			} else {
				value.get("vars")?
			}
		})?;

		let mut variables = vec![];
		let mut top_variables = vec![]; // These fields get displayed on top of all others

		for i in 1..=vars.len() {
			let name = vars.get(i)?.as_string()?;
			let value = value.get(name.as_str())?;
			let variable = self.value_to_variable(state, name, &value);
			if variable.name == "type" {
				top_variables.push(variable);
			} else {
				variables.push(variable);
			}
		}

		//top_variables.sort_by_key(|a| a.name.to_lowercase());
		variables.sort_by_key(|a| a.name.to_lowercase());
		top_variables.append(&mut variables);

		Ok(top_variables)
	}

	fn get_stack<'a>(&self, state: &'a State, stack_id: u32) -> Option<&'a Vec<debug::StackFrame>> {
		let stack_id = stack_id as usize;

		let stacks = &state.stacks;

		if stack_id == 0 {
			return Some(&stacks.active);
		}

		stacks.suspended.get(stack_id - 1)
	}

	fn get_stack_base_frame_id(&self, state: &State, stack_id: u32) -> u32 {
		let stack_id = stack_id as usize;
		let stacks = &state.stacks;

		if stack_id == 0 {
			return 0;
		}

		let mut current_base = stacks.active.len();

		for frame in &stacks.suspended[..stack_id - 1] {
			current_base += frame.len();
		}

		current_base as u32
	}

	fn get_stack_frame<'a>(
		&self,
		state: &'a State,
		frame_index: u32,
	) -> Option<&'a debug::StackFrame> {
		let mut frame_index = frame_index as usize;
		let stacks = &state.stacks;

		if frame_index < stacks.active.len() {
			return Some(&stacks.active[frame_index]);
		}

		frame_index -= stacks.active.len();

		for frame in &stacks.suspended {
			if frame_index < frame.len() {
				return Some(&frame[frame_index]);
			}

			frame_index -= frame.len();
		}

		None
	}

	fn get_args(&mut self, state: &State, frame_index: u32) -> Vec<Variable> {
		match self.get_stack_frame(state, frame_index) {
			Some(frame) => {
				let mut vars = vec![
					self.value_to_variable(state, "src".to_owned(), &frame.src),
					self.value_to_variable(state, "usr".to_owned(), &frame.usr),
				];

				let mut unnamed_count = 0;
				for (name, value) in &frame.args {
					let name = match name {
						Some(name) => String::from(name),
						None => {
							unnamed_count += 1;
							format!("undefined argument #{}", unnamed_count)
						}
					};
					vars.push(self.value_to_variable(state, name, value));
				}

				if self.should_show_internals {
					let stack_ref = state.get_ref(Variables::Internals { frame: frame_index });
					vars.push(Variable {
						name: "BYOND Internals".into(),
						value: "".into(),
						variables: Some(stack_ref),
					});
				}

				vars
			}

			None => {
				self.notify(format!(
					"tried to read arguments from invalid frame id: {}",
					frame_index
				));
				vec![]
			}
		}
	}

	fn get_locals(&mut self, state: &State, frame_index: u32) -> Vec<Variable> {
		match self.get_stack_frame(state, frame_index) {
			Some(frame) => {
				let mut vars = vec![self.value_to_variable(state, ".".to_owned(), &frame.dot)];

				for (name, local) in &frame.locals {
					vars.push(self.value_to_variable(state, String::from(name), &local));
				}

				vars
			}

			None => {
				self.notify(format!(
					"tried to read locals from invalid frame id: {}",
					frame_index
				));
				vec![]
			}
		}
	}

	fn get_vm_stack(&mut self, state: &State, frame_index: u32) -> Vec<Variable> {
		match self.get_stack_frame(state, frame_index) {
			Some(frame) => frame
				.stack
				.iter()
				.enumerate()
				.map(|(i, v)| self.value_to_variable(state, format!("[{}]", i), v))
				.collect(),

			None => {
				self.notify(format!(
					"tried to read vm stack from invalid frame id: {}",
					frame_index
				));
				vec![]
			}
		}
	}

	fn handle_breakpoint_set(&mut self, instruction: InstructionRef) {
		let line = self.get_line_number(instruction.proc.clone(), instruction.offset);

		match auxtools::Proc::find_override(instruction.proc.path, instruction.proc.override_id) {
			Some(proc) => match hook_instruction(&proc, instruction.offset) {
				Ok(()) => {
					self.send_or_disconnect(Response::BreakpointSet {
						result: BreakpointSetResult::Success { line },
					});
				}

				Err(_) => {
					self.send_or_disconnect(Response::BreakpointSet {
						result: BreakpointSetResult::Failed,
					});
				}
			},

			None => {
				self.send_or_disconnect(Response::BreakpointSet {
					result: BreakpointSetResult::Failed,
				});
			}
		}
	}

	fn handle_breakpoint_unset(&mut self, instruction: InstructionRef) {
		match auxtools::Proc::find_override(instruction.proc.path, instruction.proc.override_id) {
			Some(proc) => match unhook_instruction(&proc, instruction.offset) {
				Ok(()) => {
					self.send_or_disconnect(Response::BreakpointUnset { success: true });
				}

				Err(_) => {
					self.send_or_disconnect(Response::BreakpointUnset { success: false });
				}
			},

			None => {
				self.send_or_disconnect(Response::BreakpointUnset { success: false });
			}
		}
	}

	fn handle_stacks(&mut self, state: Option<&State>) {
		let stacks = match state {
			Some(state) => {
				let mut ret = vec![];
				ret.push(Stack {
					id: 0,
					name: state.stacks.active[0].proc.path.clone(),
				});

				for (idx, stack) in state.stacks.suspended.iter().enumerate() {
					ret.push(Stack {
						id: (idx + 1) as u32,
						name: stack[0].proc.path.clone(),
					});
				}

				ret
			}

			None => vec![],
		};

		self.send_or_disconnect(Response::Stacks { stacks });
	}

	fn handle_stack_frames(
		&mut self,
		state: &State,
		stack_id: u32,
		start_frame: Option<u32>,
		count: Option<u32>,
	) {
		let response = match self.get_stack(state, stack_id) {
			Some(stack) => {
				let frame_base = self.get_stack_base_frame_id(state, stack_id);
				let start_frame = start_frame.unwrap_or(0);
				let end_frame = start_frame + count.unwrap_or(stack.len() as u32);

				let start_frame = start_frame as usize;
				let end_frame = end_frame as usize;

				let mut frames = vec![];

				for i in start_frame..end_frame {
					if i >= stack.len() {
						break;
					}

					let proc_ref = ProcRef {
						path: stack[i].proc.path.to_owned(),
						override_id: stack[i].proc.override_id(),
					};

					frames.push(StackFrame {
						id: frame_base + (i as u32),
						instruction: InstructionRef {
							proc: proc_ref.clone(),
							offset: stack[i].offset as u32,
						},
						line: self.get_line_number(proc_ref, stack[i].offset as u32),
					});
				}

				Response::StackFrames {
					frames,
					total_count: stack.len() as u32,
				}
			}

			None => {
				self.notify("received StackFrames request when not paused");
				Response::StackFrames {
					frames: vec![],
					total_count: 0,
				}
			}
		};

		self.send_or_disconnect(response);
	}

	fn handle_scopes(&mut self, state: &State, frame_id: u32) {
		let arguments = Variables::Arguments { frame: frame_id };
		let locals = Variables::Locals { frame: frame_id };

		let globals_value = Value::globals();
		let globals = unsafe {
			Variables::ObjectVars {
				tag: globals_value.value.tag as u8,
				data: globals_value.value.data.id,
			}
		};

		let response = Response::Scopes {
			arguments: Some(state.get_ref(arguments)),
			locals: Some(state.get_ref(locals)),
			globals: Some(state.get_ref(globals)),
		};

		self.send_or_disconnect(response);
	}

	fn handle_variables(&mut self, state: &State, vars: VariablesRef) {
		let response = match state.get_variables(vars) {
			Some(vars) => match vars {
				Variables::Arguments { frame } => Response::Variables {
					vars: self.get_args(state, frame),
				},
				Variables::Locals { frame } => Response::Variables {
					vars: self.get_locals(state, frame),
				},
				Variables::ObjectVars { tag, data } => {
					let value = unsafe {
						Value::from_raw(raw_types::values::Value {
							tag: std::mem::transmute(tag),
							data: ValueData { id: data },
						})
					};

					match self.object_to_variables(state, &value) {
						Ok(vars) => Response::Variables { vars },

						Err(e) => {
							self.notify(format!(
								"runtime occured while processing Variables request: {:?}",
								e
							));
							Response::Variables { vars: vec![] }
						}
					}
				}
				Variables::ListContents { tag, data } => {
					let value = unsafe {
						Value::from_raw(raw_types::values::Value {
							tag: std::mem::transmute(tag),
							data: ValueData { id: data },
						})
					};

					match self.list_to_variables(state, &value) {
						Ok(vars) => Response::Variables { vars },

						Err(e) => {
							self.notify(format!(
								"runtime occured while processing Variables request: {:?}",
								e
							));
							Response::Variables { vars: vec![] }
						}
					}
				}

				Variables::ListPair {
					key_tag,
					key_data,
					value_tag,
					value_data,
				} => {
					let key = unsafe {
						Value::from_raw(raw_types::values::Value {
							tag: std::mem::transmute(key_tag),
							data: ValueData { id: key_data },
						})
					};

					let value = unsafe {
						Value::from_raw(raw_types::values::Value {
							tag: std::mem::transmute(value_tag),
							data: ValueData { id: value_data },
						})
					};

					Response::Variables {
						vars: vec![
							self.value_to_variable(state, "key".to_owned(), &key),
							self.value_to_variable(state, "value".to_owned(), &value),
						],
					}
				}

				Variables::Internals { frame } => {
					let stack_ref = state.get_ref(Variables::Stack { frame });

					let stack_data = self.get_stack_frame(state, frame).unwrap();

					Response::Variables {
						vars: vec![
							Variable {
								name: "Stack".into(),
								value: "".into(),
								variables: Some(stack_ref),
							},
							self.value_to_variable(state, "Cache".into(), &stack_data.cache),
						],
					}
				}

				Variables::Stack { frame } => Response::Variables {
					vars: self.get_vm_stack(state, frame),
				},
			},

			None => {
				self.notify("received unknown VariableRef in Variables request");
				Response::Variables { vars: vec![] }
			}
		};

		self.send_or_disconnect(response);
	}

	fn handle_command(
		&mut self,
		state: Option<&State>,
		frame_id: Option<u32>,
		command: &str,
	) -> String {
		// How many matches variables can you spot? It could be better...
		let response = match self
			.app
			.get_matches_from_safe_borrow(command.split_ascii_whitespace())
		{
			Ok(matches) => {
				match matches.subcommand() {
					("disassemble", Some(matches)) => {
						if let Some(proc) = matches.value_of("proc") {
							// Default id to 0 in the worst way possible
							let id = matches
								.value_of("id")
								.and_then(|x| x.parse::<u32>().ok())
								.unwrap_or(0);

							self.handle_disassemble(proc, id, None)
						} else if let Some(frame_id) = frame_id {
							if let Some(frame) = self.get_stack_frame(state.unwrap(), frame_id) {
								let proc = frame.proc.path.clone();
								let id = frame.proc.override_id();
								self.handle_disassemble(&proc, id, Some(frame.offset))
							} else {
								"couldn't find stack frame (is execution not paused?)".to_owned()
							}
						} else {
							"no execution frame selected".to_owned()
						}
					}

					_ => "unknown command".to_owned(),
				}
			}
			Err(e) => e.message,
		};

		response
	}

	fn handle_eval(&mut self, state: Option<&State>, frame_id: Option<u32>, command: &str) {
		if command.starts_with('#') {
			let response = self.handle_command(state, frame_id, &command[1..]);
			self.send_or_disconnect(Response::Eval(response));
			return;
		}

		self.send_or_disconnect(Response::Eval(
			"Auxtools can't currently evaluate DM. To see available commands, use `#help`"
				.to_owned(),
		));
	}

	fn handle_disassemble(&mut self, path: &str, id: u32, current_offset: Option<u32>) -> String {
		let response = match auxtools::Proc::find_override(path, id) {
			Some(proc) => {
				// Make sure to temporarily remove all breakpoints in this proc
				let breaks = get_hooked_offsets(&proc);

				for offset in &breaks {
					unhook_instruction(&proc, *offset).unwrap();
				}

				let dism = proc.disassemble(current_offset);

				for offset in &breaks {
					hook_instruction(&proc, *offset).unwrap();
				}

				format!("Dism for {:?}\n{}", proc, dism)
			}

			None => "Proc not found".to_owned(),
		};

		return response;
	}

	// returns true if we need to break
	fn handle_request(&mut self, state: Option<&State>, request: Request) -> bool {
		match request {
			Request::Disconnect => unreachable!(),
			Request::CatchRuntimes { should_catch } => self.should_catch_runtimes = should_catch,
			Request::BreakpointSet { instruction } => self.handle_breakpoint_set(instruction),
			Request::BreakpointUnset { instruction } => self.handle_breakpoint_unset(instruction),
			Request::Stacks => self.handle_stacks(state),
			Request::Scopes { frame_id } => self.handle_scopes(state.unwrap(), frame_id),
			Request::Variables { vars } => self.handle_variables(state.unwrap(), vars),
			Request::Eval { frame_id, command } => self.handle_eval(state, frame_id, &command),

			Request::StackFrames {
				stack_id,
				start_frame,
				count,
			} => self.handle_stack_frames(state.unwrap(), stack_id, start_frame, count),

			Request::LineNumber { proc, offset } => {
				self.send_or_disconnect(Response::LineNumber {
					line: self.get_line_number(proc, offset),
				});
			}

			Request::Offset { proc, line } => {
				self.send_or_disconnect(Response::Offset {
					offset: self.get_offset(proc, line),
				});
			}

			Request::Pause => {
				self.send_or_disconnect(Response::Ack);
				return true;
			}

			Request::StdDef => {
				let stddef = crate::stddef::get_stddef().map(|x| x.to_string());
				self.send_or_disconnect(Response::StdDef(stddef));
			}

			Request::CurrentInstruction { frame_id } => {
				let response = match self.get_stack_frame(state.unwrap(), frame_id) {
					Some(frame) => Some(InstructionRef {
						proc: ProcRef {
							path: frame.proc.path.to_owned(),
							override_id: frame.proc.override_id(),
						},
						offset: frame.offset as u32,
					}),

					None => None,
				};

				self.send_or_disconnect(Response::CurrentInstruction(response));
			}

			// The following requests are special cases and handled outside of this function
			Request::Configured | Request::Continue { .. } => {
				self.send_or_disconnect(Response::Ack);
			}
		}

		false
	}

	fn check_connected(&mut self) -> bool {
		match &self.stream {
			ServerStream::Disconnected => false,
			ServerStream::Connected(_) => true,
			ServerStream::Waiting(receiver) => {
				if let Ok(stream) = receiver.try_recv() {
					self.stream = ServerStream::Connected(stream);
					true
				} else {
					false
				}
			}
		}
	}

	fn wait_for_connection(&mut self) {
		match &self.stream {
			ServerStream::Waiting(receiver) => {
				if let Ok(stream) = receiver.recv() {
					self.stream = ServerStream::Connected(stream);
				}
			}

			_ => (),
		}
	}

	fn notify<T: Into<String>>(&mut self, message: T) {
		let message = message.into();
		eprintln!("Debug Server: {:?}", message);

		if !self.check_connected() {
			return;
		}

		self.send_or_disconnect(Response::Notification { message });
	}

	pub fn handle_breakpoint(
		&mut self,
		_ctx: *mut raw_types::procs::ExecutionContext,
		reason: BreakpointReason,
	) -> ContinueKind {
		// Ignore all breakpoints unless we're connected
		if !self.check_connected() {
			return ContinueKind::Continue;
		}

		if let BreakpointReason::Runtime(_) = reason {
			if !self.should_catch_runtimes {
				return ContinueKind::Continue;
			}
		}

		self.notify(format!("Pausing execution (reason: {:?})", reason));

		let state = State::new();

		self.send_or_disconnect(Response::BreakpointHit { reason });

		while let Ok(request) = self.requests.recv() {
			// Hijack and handle any Continue requests
			if let Request::Continue { kind } = request {
				self.send_or_disconnect(Response::Ack);
				return kind;
			}

			// if we get a pause request here we can ignore it
			self.handle_request(Some(&state), request);
		}

		ContinueKind::Continue
	}

	// returns true if we need to pause
	pub fn process(&mut self) -> bool {
		// Don't do anything until we're connected
		if !self.check_connected() {
			return false;
		}

		let mut should_pause = false;

		while let Ok(request) = self.requests.try_recv() {
			should_pause = should_pause || self.handle_request(None, request);
		}

		should_pause
	}

	/// Block while processing all received requests normally until the debug client is configured
	pub fn process_until_configured(&mut self) {
		self.wait_for_connection();

		while let Ok(request) = self.requests.recv() {
			if let Request::Configured = request {
				self.send_or_disconnect(Response::Ack);
				break;
			}

			self.handle_request(None, request);
		}
	}

	fn send_or_disconnect(&mut self, response: Response) {
		match self.stream {
			ServerStream::Connected(_) => match self.send(response) {
				Ok(_) => {}
				Err(e) => {
					eprintln!("Debug server failed to send message: {}", e);
					self.disconnect();
				}
			},

			ServerStream::Waiting(_) | ServerStream::Disconnected => {
				unreachable!("Debug Server is not connected")
			}
		}
	}

	fn disconnect(&mut self) {
		if let ServerStream::Connected(stream) = &mut self.stream {
			eprintln!("Debug server disconnecting");
			let data = bincode::serialize(&Response::Disconnect).unwrap();
			let _ = stream.write_all(&(data.len() as u32).to_le_bytes());
			let _ = stream.write_all(&data[..]);
			let _ = stream.flush();
			let _ = stream.shutdown(std::net::Shutdown::Both);
		}

		self.stream = ServerStream::Disconnected;
	}

	fn send(&mut self, response: Response) -> Result<(), Box<dyn std::error::Error>> {
		if let ServerStream::Connected(stream) = &mut self.stream {
			let data = bincode::serialize(&response)?;
			stream.write_all(&(data.len() as u32).to_le_bytes())?;
			stream.write_all(&data[..])?;
			stream.flush()?;
			return Ok(());
		}

		unreachable!();
	}
}

impl Drop for Server {
	fn drop(&mut self) {
		self.disconnect();
	}
}

impl ServerThread {
	fn spawn_listener(
		self,
		listener: TcpListener,
		connection_sender: mpsc::Sender<TcpStream>,
	) -> JoinHandle<()> {
		thread::spawn(move || match listener.accept() {
			Ok((stream, _)) => {
				match connection_sender.send(stream.try_clone().unwrap()) {
					Ok(_) => {}
					Err(e) => {
						eprintln!("Debug server thread failed to pass cloned TcpStream: {}", e);
						return;
					}
				}

				self.run(stream);
			}

			Err(e) => {
				eprintln!("Debug server failed to accept connection: {}", e);
			}
		})
	}

	// returns true if we should disconnect
	fn handle_request(&mut self, data: &[u8]) -> Result<bool, Box<dyn Error>> {
		let request = bincode::deserialize::<Request>(data)?;

		if let Request::Disconnect = request {
			return Ok(true);
		}

		self.requests.send(request)?;
		Ok(false)
	}

	fn run(mut self, mut stream: TcpStream) {
		let mut buf = vec![];

		// The incoming stream is a u32 followed by a bincode-encoded Request.
		loop {
			let mut len_bytes = [0u8; 4];
			let len = match stream.read_exact(&mut len_bytes) {
				Ok(_) => u32::from_le_bytes(len_bytes),

				Err(e) => {
					eprintln!("Debug server thread read error: {}", e);
					break;
				}
			};

			buf.resize(len as usize, 0);
			match stream.read_exact(&mut buf) {
				Ok(_) => (),

				Err(e) => {
					eprintln!("Debug server thread read error: {}", e);
					break;
				}
			};

			match self.handle_request(&buf[..]) {
				Ok(requested_disconnect) => {
					if requested_disconnect {
						eprintln!("Debug client disconnected");
						break;
					}
				}

				Err(e) => {
					eprintln!("Debug server thread failed to handle request: {}", e);
					break;
				}
			}
		}

		eprintln!("Debug server thread finished");
	}
}
