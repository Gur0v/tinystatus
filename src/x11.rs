use std::io;
use std::sync::mpsc::SyncSender;

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xkb::{
    self, EventType, ID, MapPart, NameDetail, SelectEventsAux, SelectEventsAuxStateNotify,
    StatePart,
};
use x11rb::protocol::xproto::ConnectionExt;
use x11rb::rust_connection::RustConnection;

use crate::{layout, status::Update, watch};

fn error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

fn connect() -> io::Result<RustConnection> {
    let (connection, _) = x11rb::connect(None).map_err(error)?;
    if !xkb::use_extension(&connection, 1, 0)
        .map_err(error)?
        .reply()
        .map_err(error)?
        .supported
    {
        return Err(error("XKB is unavailable"));
    }
    Ok(connection)
}

fn group_atom(mask: u8, group: usize, groups: &[u32]) -> Option<&u32> {
    let bit = 1u8.checked_shl(group.try_into().ok()?)?;
    if mask & bit == 0 {
        return None;
    }
    groups.get((mask & (bit - 1)).count_ones() as usize)
}

fn query(connection: &RustConnection) -> io::Result<String> {
    let device = ID::USE_CORE_KBD.into();
    let state = xkb::get_state(connection, device)
        .map_err(error)?
        .reply()
        .map_err(error)?;
    let names = xkb::get_names(connection, device, NameDetail::GROUP_NAMES)
        .map_err(error)?
        .reply()
        .map_err(error)?;
    let group = usize::from(u8::from(state.group));
    let mask = u8::from(names.group_names);
    let atom = names
        .value_list
        .groups
        .as_ref()
        .and_then(|groups| group_atom(mask, group, groups))
        .copied()
        .unwrap_or(0);
    if atom == 0 {
        return Ok("??".into());
    }
    let name = connection
        .get_atom_name(atom)
        .map_err(error)?
        .reply()
        .map_err(error)?;
    Ok(layout::code(&String::from_utf8_lossy(&name.name)))
}

pub fn current() -> io::Result<String> {
    query(&connect()?)
}

pub fn watch(tx: SyncSender<Update>) {
    watch::watch(
        tx,
        "x11",
        || Update::Layout("??".into()),
        |tx| -> io::Result<()> {
            let connection = connect()?;
            xkb::select_events(
                &connection,
                ID::USE_CORE_KBD.into(),
                EventType::from(0u16),
                EventType::NEW_KEYBOARD_NOTIFY | EventType::NAMES_NOTIFY,
                MapPart::from(0u16),
                MapPart::from(0u16),
                &SelectEventsAux {
                    state_notify: Some(SelectEventsAuxStateNotify {
                        affect_state: StatePart::GROUP_STATE,
                        state_details: StatePart::GROUP_STATE,
                    }),
                    ..Default::default()
                },
            )
            .map_err(error)?
            .check()
            .map_err(error)?;
            let mut previous = query(&connection)?;
            tx.send(Update::Layout(previous.clone())).map_err(error)?;
            loop {
                if matches!(
                    connection.wait_for_event().map_err(error)?,
                    Event::XkbStateNotify(_)
                        | Event::XkbNamesNotify(_)
                        | Event::XkbNewKeyboardNotify(_)
                ) {
                    let layout = query(&connection)?;
                    if layout != previous {
                        tx.send(Update::Layout(layout.clone())).map_err(error)?;
                        previous = layout;
                    }
                }
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::group_atom;

    #[test]
    fn selects_named_groups_by_mask() {
        assert_eq!(group_atom(0b101, 0, &[10, 20]), Some(&10));
        assert_eq!(group_atom(0b101, 1, &[10, 20]), None);
        assert_eq!(group_atom(0b101, 2, &[10, 20]), Some(&20));
        assert_eq!(group_atom(0b101, 2, &[10]), None);
        assert_eq!(group_atom(0xff, 8, &[10]), None);
    }
}
