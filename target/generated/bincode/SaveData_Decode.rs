impl < __Context > :: bincode :: Decode < __Context > for SaveData
{
    fn decode < __D : :: bincode :: de :: Decoder < Context = __Context > >
    (decoder : & mut __D) ->core :: result :: Result < Self, :: bincode ::
    error :: DecodeError >
    {
        core :: result :: Result ::
        Ok(Self
        {
            items_granted : :: bincode :: Decode :: decode(decoder) ?,
            locations : :: bincode :: Decode :: decode(decoder) ?, seed : ::
            bincode :: Decode :: decode(decoder) ?,
            local_virtual_items_granted : :: bincode :: Decode ::
            decode(decoder) ?, foreign_virtual_items_notified : :: bincode ::
            Decode :: decode(decoder) ?,
        })
    }
} impl < '__de, __Context > :: bincode :: BorrowDecode < '__de, __Context >
for SaveData
{
    fn borrow_decode < __D : :: bincode :: de :: BorrowDecoder < '__de,
    Context = __Context > > (decoder : & mut __D) ->core :: result :: Result <
    Self, :: bincode :: error :: DecodeError >
    {
        core :: result :: Result ::
        Ok(Self
        {
            items_granted : :: bincode :: BorrowDecode ::< '_, __Context >::
            borrow_decode(decoder) ?, locations : :: bincode :: BorrowDecode
            ::< '_, __Context >:: borrow_decode(decoder) ?, seed : :: bincode
            :: BorrowDecode ::< '_, __Context >:: borrow_decode(decoder) ?,
            local_virtual_items_granted : :: bincode :: BorrowDecode ::< '_,
            __Context >:: borrow_decode(decoder) ?,
            foreign_virtual_items_notified : :: bincode :: BorrowDecode ::<
            '_, __Context >:: borrow_decode(decoder) ?,
        })
    }
}