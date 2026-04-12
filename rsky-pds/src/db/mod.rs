use diesel::pg::PgConnection;
use rocket_sync_db_pools::database;
use std::fmt::{Debug, Formatter};

#[database("pg_db")]
pub struct DbConn(PgConnection);

impl Debug for DbConn {
    fn fmt(&self, _f: &mut Formatter<'_>) -> std::fmt::Result {
        todo!()
    }
}
