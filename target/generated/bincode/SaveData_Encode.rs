impl :: bincode :: Encode for SaveData
{
    fn encode < __E : :: bincode :: enc :: Encoder >
    (& self, encoder : & mut __E) ->core :: result :: Result < (), :: bincode
    :: error :: EncodeError >
    {
        :: bincode :: Encode :: encode(&self.items_granted, encoder) ?; ::
        bincode :: Encode :: encode(&self.locations, encoder) ?; :: bincode ::
        Encode :: encode(&self.seed, encoder) ?; :: bincode :: Encode ::
        encode(&self.local_virtual_items_granted, encoder) ?; :: bincode ::
        Encode :: encode(&self.foreign_virtual_items_notified, encoder) ?;
        core :: result :: Result :: Ok(())
    }
}