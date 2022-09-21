use proc_macro2::TokenStream;
use quote::{quote, ToTokens};

use crate::utils::helpers::*;
use crate::utils::macro_tokens::MacroTokens;

/// Generates the TokenStream for the _insert() CRUD operation
pub fn generate_insert_tokens(macro_data: &MacroTokens) -> TokenStream {
    let (vis, ty) = (macro_data.vis, macro_data.ty);

    // Gets the name of the table in the database that maps the annotated Struct
    let table_name = database_table_name_from_struct(ty);

    // Retrieves the fields of the Struct as continuous String
    let column_names = macro_data.get_struct_fields_as_strings();

    // Retrives the fields of the Struct
    let fields = macro_data.get_struct_fields();

    let insert_values = fields.iter().map( |ident| {
        quote! { &self.#ident }
    });
    let insert_values_cloned = insert_values.clone();

    let pk = macro_data.get_primary_key_annotation()
        .unwrap_or_default();
    
    let pk_ident_type = macro_data._fields_with_types()
        .into_iter()
        .find( |(i, _t)| i.to_string() == pk);
    
    let pk_ident = if let Some(pk_data) = &pk_ident_type {
        let i = &pk_data.0;
        quote! { #i }
    } else {
        // If there's no pk annotation, Canyon won't generate the delete CRUD operation as a method of the implementor.
        return quote! {};
    };

    let pk_type = if let Some(pk_data) = &pk_ident_type {
        let t = &pk_data.1;
        quote! { #t }
    } else { 
        // If there's no pk annotation, Canyon won't generate the delete CRUD operation as a method of the implementor.
        return quote! {}; 
    };



    quote! {
        /// Inserts into a database entity the current data in `self`, generating a new
        /// entry (row), returning the value of the autogenerated `PRIMARY KEY` = `self.<pk_field>`
        /// 
        /// This `insert` operation needs a `&mut` reference. That's because typically, 
        /// an insert operation represents *new* data stored in the database, so, when
        /// inserted, the database will generate a unique new value for the 
        /// `pk` field, having a unique identifier for every record, and it will
        /// automatically assign that returned id to `self.<pk_field>`. So, after the `insert`
        /// operation, you instance will have the value of the *PRIMARY KEY*
        /// of the database row that represents.
        /// 
        /// ## *Examples*
        /// ```
        
        /// let mut lec: League = League {
        ///     id: Default::default(),
        ///     ext_id: 1,
        ///     slug: "LEC".to_string(),
        ///     name: "League Europe Champions".to_string(),
        ///     region: "EU West".to_string(),
        ///     image_url: "https://lec.eu".to_string(),
        /// };

        /// let mut lck: League = League {
        ///     id: Default::default(),
        ///     ext_id: 2,
        ///     slug: "LCK".to_string(),
        ///     name: "League Champions Korea".to_string(),
        ///     region: "South Korea".to_string(),
        ///     image_url: "https://korean_lck.kr".to_string(),
        /// };

        /// let mut lpl: League = League {
        ///     id: Default::default(),
        ///     ext_id: 3,
        ///     slug: "LPL".to_string(),
        ///     name: "League PRO China".to_string(),
        ///     region: "China".to_string(),
        ///     image_url: "https://chinese_lpl.ch".to_string(),
        /// };

        /// Now, the insert operations in Canyon is designed as a method over
        /// the object, so the data of the instance is automatically parsed
        /// into it's correct types and formats and inserted into the table
        /// lec.insert().await;
        /// lck.insert().await;
        /// lpl.insert().await;
        /// 
        /// Remember that after the insert operation, your instance will have updated
        /// the value of the field declared as `primary_key`
        /// ```
        #vis async fn insert(&mut self) -> () {
            self.#pk_ident = <#ty as canyon_sql::canyon_crud::crud::CrudOperations<#ty>>::__insert(
                #table_name,
                #pk,
                &mut #column_names, 
                &[#(#insert_values),*],
                ""
            ).await
            .ok()
            .expect(
                format!(
                    "Insert operation failed for {:?}", 
                    &self
                ).as_str()
            ).wrapper
            .get(0)
            .unwrap()
            .get::<&str, #pk_type>(#pk)
            .to_owned();
        }

        /// Inserts into a database entity the current data in `self`, generating a new
        /// entry (row), returning the `PRIMARY KEY` = `self.<pk_field>` with the specified
        /// datasource by it's `datasouce name`, defined in the configuration file.
        /// 
        /// This `insert` operation needs a `&mut` reference. That's because typically, 
        /// an insert operation represents *new* data stored in the database, so, when
        /// inserted, the database will generate a unique new value for the  
        /// `pk` field, having a unique identifier for every record, and it will
        /// automatically assign that returned pk to `self.<pk_field>`. So, after the `insert`
        /// operation, you instance will have the correct value that is the *PRIMARY KEY*
        /// of the database row that represents.
        /// 
        /// ## *Examples*
        /// ```
        
        /// let mut lec: League = League {
        ///     id: Default::default(),
        ///     ext_id: 1,
        ///     slug: "LEC".to_string(),
        ///     name: "League Europe Champions".to_string(),
        ///     region: "EU West".to_string(),
        ///     image_url: "https://lec.eu".to_string(),
        /// };

        /// let mut lck: League = League {
        ///     id: Default::default(),
        ///     ext_id: 2,
        ///     slug: "LCK".to_string(),
        ///     name: "League Champions Korea".to_string(),
        ///     region: "South Korea".to_string(),
        ///     image_url: "https://korean_lck.kr".to_string(),
        /// };

        /// let mut lpl: League = League {
        ///     id: Default::default(),
        ///     ext_id: 3,
        ///     slug: "LPL".to_string(),
        ///     name: "League PRO China".to_string(),
        ///     region: "China".to_string(),
        ///     image_url: "https://chinese_lpl.ch".to_string(),
        /// };

        /// Now, the insert operations in Canyon is designed as a method over
        /// the object, so the data of the instance is automatically parsed
        /// into it's correct types and formats and inserted into the table
        /// lec.insert().await;
        /// lck.insert().await;
        /// lpl.insert().await;
        /// 
        /// Remember that after the insert operation, your instance will have updated
        /// the value of the field declared as `primary_key`
        /// ```
        #vis async fn insert_datasource(&mut self, datasource_name: &str) -> () {
            self.#pk_ident = <#ty as canyon_sql::canyon_crud::crud::CrudOperations<#ty>>::__insert(
                #table_name,
                #pk,
                &mut #column_names, 
                &[#(#insert_values_cloned),*],
                datasource_name
            ).await
            .ok()
            .expect(
                format!(
                    "Insert operation failed for {:?}", 
                    &self
                ).as_str()
            ).wrapper
            .get(0)
            .unwrap()
            .get::<&str, #pk_type>(#pk)
            .to_owned();
        }
    }
}


/// Generates the TokenStream for the _insert_result() CRUD operation
pub fn generate_insert_result_tokens(macro_data: &MacroTokens) -> TokenStream {

    // Destructure macro_tokens into raw data
    let (vis, ty) = (macro_data.vis, macro_data.ty);

    // Gets the name of the table in the database that maps the annotated Struct
    let table_name = database_table_name_from_struct(ty);

    // Retrieves the fields of the Struct as continuous String
    let column_names = macro_data.get_struct_fields_as_strings();

    // Retrives the fields of the Struct
    let fields = macro_data.get_struct_fields();

    let insert_values = fields.iter().map( |ident| {
        quote! { &self.#ident }
    });
    let insert_values_cloned = insert_values.clone();

    let pk = macro_data.get_primary_key_annotation()
        .unwrap_or_default();

    let pk_ident_type = macro_data._fields_with_types()
        .into_iter()
        .find( |(i, _t)| i.to_string() == pk);

    let pk_ident = if let Some(pk_data) = &pk_ident_type {
        let i = &pk_data.0;
        quote! { #i }
    } else {
        // If there's no pk annotation, Canyon won't generate the delete CRUD operation as a method of the implementor.
        return quote! {};
    };

    let pk_type = if let Some(pk_type_) = &pk_ident_type {
        let t = &pk_type_.1;
        quote! { #t }
    } else {
        // If there's no pk annotation, Canyon won't generate the delete CRUD operation as a method of the implementor.
        return quote! {};
    };

    quote! {
        /// Inserts into a database entity the current data in `self`, generating a new
        /// entry (row), returning the `PRIMARY KEY` = `self.<pk_field>` with the specified
        /// datasource by it's `datasouce name`, defined in the configuration file.
        /// 
        /// This `insert` operation needs a `&mut` reference. That's because typically, 
        /// an insert operation represents *new* data stored in the database, so, when
        /// inserted, the database will generate a unique new value for the  
        /// `pk` field, having a unique identifier for every record, and it will
        /// automatically assign that returned pk to `self.<pk_field>`. So, after the `insert`
        /// operation, you instance will have the correct value that is the *PRIMARY KEY*
        /// of the database row that represents.
        /// 
        /// This operation returns a result type, indicating a posible failure querying the database.
        /// 
        /// ## *Examples*
        ///```
        /// let mut lec: League = League {
        ///     id: Default::default(),
        ///     ext_id: 1,
        ///     slug: "LEC".to_string(),
        ///     name: "League Europe Champions".to_string(),
        ///     region: "EU West".to_string(),
        ///     image_url: "https://lec.eu".to_string(),
        /// };
        ///
        /// println!("LEC before: {:?}", &lec);
        ///
        /// let ins_result = lec.insert_result().await;
        ///
        /// Now, we can handle the result returned, because it can contains a
        /// critical error that may leads your program to panic
        /// if let Ok(_) = ins_result {
        ///     println!("LEC after: {:?}", &lec);
        /// } else {
        ///     eprintln!("{:?}", ins_result.err())
        /// }
        /// ```
        /// 
        #vis async fn insert_result(&mut self) 
            -> Result<(), Box<dyn std::error::Error + Sync + std::marker::Send>> 
        {
            let result = <#ty as canyon_sql::canyon_crud::crud::CrudOperations<#ty>>::__insert(
                #table_name,
                #pk,
                #column_names, 
                &[#(#insert_values),*],
                ""
            ).await;

            if let Err(error) = result {
                Err(error)
            } else {
                self.#pk_ident = result  
                    .ok()
                    .expect(
                        format!(
                            "Insert operation failed for {:?}", 
                            &self
                        ).as_str()
                    ).wrapper
                    .get(0)
                    .unwrap()
                    .get::<&str, #pk_type>(#pk);

                Ok(())
            }
        }

        /// Inserts into a database entity the current data in `self`, generating a new
        /// entry (row), returning the `PRIMARY KEY` = `self.<pk_field>` with the specified
        /// datasource by it's `datasouce name`, defined in the configuration file.
        /// 
        /// This `insert` operation needs a `&mut` reference. That's because typically, 
        /// an insert operation represents *new* data stored in the database, so, when
        /// inserted, the database will generate a unique new value for the  
        /// `pk` field, having a unique identifier for every record, and it will
        /// automatically assign that returned pk to `self.<pk_field>`. So, after the `insert`
        /// operation, you instance will have the correct value that is the *PRIMARY KEY*
        /// of the database row that represents.
        /// 
        /// This operation returns a result type, indicating a posible failure querying the database.
        /// 
        /// ## *Examples*
        ///```
        /// let mut lec: League = League {
        ///     id: Default::default(),
        ///     ext_id: 1,
        ///     slug: "LEC".to_string(),
        ///     name: "League Europe Champions".to_string(),
        ///     region: "EU West".to_string(),
        ///     image_url: "https://lec.eu".to_string(),
        /// };
        ///
        /// println!("LEC before: {:?}", &lec);
        ///
        /// let ins_result = lec.insert_result().await;
        ///
        /// Now, we can handle the result returned, because it can contains a
        /// critical error that may leads your program to panic
        /// if let Ok(_) = ins_result {
        ///     println!("LEC after: {:?}", &lec);
        /// } else {
        ///     eprintln!("{:?}", ins_result.err())
        /// }
        /// ```
        /// 
        #vis async fn insert_result_datasource(&mut self, datasource_name: &str)
            -> Result<(), Box<dyn std::error::Error + Sync + std::marker::Send>> 
        {
            let result = <#ty as canyon_sql::canyon_crud::crud::CrudOperations<#ty>>::__insert(
                #table_name,
                #pk,
                #column_names, 
                &[#(#insert_values_cloned),*],
                datasource_name
            ).await;

            if let Err(error) = result {
                Err(error)
            } else {
                self.#pk_ident = result  
                    .ok()
                    .expect(
                        format!(
                            "Insert operation failed for {:?}", 
                            &self
                        ).as_str()
                    ).wrapper
                    .get(0)
                    .unwrap()
                    .get::<&str, #pk_type>(#pk);

                Ok(())
            }
        }
    }
}

/// Generates the TokenStream for the __insert() CRUD operation, but being available
/// as a [`QueryBuilder`] object, and instead of being a method over some [`T`] type, 
/// as an associated function for [`T`]
/// 
/// This, also lets the user to have the option to be able to insert multiple
/// [`T`] objects in only one query
pub fn generate_multiple_insert_tokens(macro_data: &MacroTokens) -> TokenStream {

    // Destructure macro_tokens into raw data
    let (vis, ty) = (macro_data.vis, macro_data.ty);

    // Gets the name of the table in the database that maps the annotated Struct
    let table_name = database_table_name_from_struct(ty);

    // Retrieves the fields of the Struct as continuous String
    let column_names = macro_data.get_struct_fields_as_strings();
    
    // Retrives the fields of the Struct
    let fields = macro_data.get_struct_fields();
    
    let macro_fields = fields.iter().map( |field| 
        quote! { &instance.#field } 
    );
    println!("macro fields: {:?}", &macro_fields);
    let macro_fields_cloned = macro_fields.clone();

    let pk = macro_data.get_primary_key_annotation()
        .unwrap_or_default();
    println!("Primary key for: {:?} => {:?}", &ty.to_string(), &pk);
    
    let pk_ident_type = macro_data._fields_with_types()
        .into_iter()
        .find( |(i, _t)| i.to_string() == pk);

    let pk_ident = if let Some(pk_data) = &pk_ident_type {
        let i = &pk_data.0;
        println!("Primary key ident: {:?}", i.to_string());
        quote! { #i }
    } else {
        // If there's no pk annotation, Canyon won't generate the delete CRUD operation as a method of the implementor.
        return quote! {};
    };

    let pk_type = if let Some(pk_type_) = pk_ident_type {
        let t = pk_type_.1;
        println!("Primary key type: {:?}\n", t.to_token_stream().to_string());
        quote! { #t }
    } else { 
        // If there's no pk annotation, Canyon won't generate the delete CRUD operation as a method of the implementor.
        return quote! {};
    };

    quote! {
        /// Inserts multiple instances of some type `T` into its related table.
        /// 
        /// ```
        /// let mut new_league = League {
        ///     id: Default::default(),
        ///    ext_id: 392489032,
        ///     slug: "League10".to_owned(),
        ///     name: "League10also".to_owned(),
        ///     region: "Turkey".to_owned(),
        ///     image_url: "https://www.sdklafjsd.com".to_owned()
        /// };
        /// let mut new_league2 = League {
        ///     id: Default::default(),
        ///     ext_id: 392489032,
        ///     slug: "League11".to_owned(),
        ///     name: "League11also".to_owned(),
        ///     region: "LDASKJF".to_owned(),
        ///     image_url: "https://www.sdklafjsd.com".to_owned()
        /// };
        /// let mut new_league3 = League {
        ///     id: Default::default(),
        ///     ext_id: 9687392489032,
        ///     slug: "League3".to_owned(),
        ///     name: "3League".to_owned(),
        ///     region: "EU".to_owned(),
        ///     image_url: "https://www.lag.com".to_owned()
        /// };
        ///
        /// League::insert_multiple(
        ///     &mut [&mut new_league, &mut new_league2, &mut new_league3]
        /// ).await
        /// .ok();
        /// ```
        #vis async fn multi_insert(values: & mut [& mut #ty]) -> (
            Result<(), Box<dyn std::error::Error + Sync + std::marker::Send>> 
        ) {
            use crate::bounds::QueryParameters;
            
            let mut final_values: Vec<Vec<&dyn QueryParameters<'_>>> = Vec::new();
            for instance in values.iter() {
                let intermediate: &[&dyn QueryParameters<'_>] = &[#(#macro_fields),*];
                
                let mut longer_lived: Vec<&dyn QueryParameters<'_>> = Vec::new();
                for value in intermediate.into_iter() {
                    longer_lived.push(*value)
                }

                final_values.push(longer_lived)
            }
            
            let autogenerated_ids = <#ty as canyon_sql::canyon_crud::crud::CrudOperations<#ty>>::__insert_multi(
                #table_name,
                #pk,
                #column_names, 
                &mut final_values,
                ""
            ).await;

            if let Err(error) = autogenerated_ids {
                Err(error)
            } else {
                for (idx, instance) in values.iter_mut().enumerate() {
                    instance.#pk_ident = autogenerated_ids
                        .as_ref()
                        .ok()
                        .unwrap()
                        .wrapper
                        .get(idx)
                        .expect("Failed getting the returned IDs for a multi insert")
                        .get::<&str, #pk_type>(#pk);
                }

                Ok(())
            }
        }

        /// Inserts multiple instances of some type `T` into its related table with the specified
        /// datasource by it's `datasouce name`, defined in the configuration file.
        /// 
        /// ```
        /// let mut new_league = League {
        ///     id: Default::default(),
        ///    ext_id: 392489032,
        ///     slug: "League10".to_owned(),
        ///     name: "League10also".to_owned(),
        ///     region: "Turkey".to_owned(),
        ///     image_url: "https://www.sdklafjsd.com".to_owned()
        /// };
        /// let mut new_league2 = League {
        ///     id: Default::default(),
        ///     ext_id: 392489032,
        ///     slug: "League11".to_owned(),
        ///     name: "League11also".to_owned(),
        ///     region: "LDASKJF".to_owned(),
        ///     image_url: "https://www.sdklafjsd.com".to_owned()
        /// };
        /// let mut new_league3 = League {
        ///     id: Default::default(),
        ///     ext_id: 9687392489032,
        ///     slug: "League3".to_owned(),
        ///     name: "3League".to_owned(),
        ///     region: "EU".to_owned(),
        ///     image_url: "https://www.lag.com".to_owned()
        /// };
        ///
        /// League::insert_multiple(
        ///     &mut [&mut new_league, &mut new_league2, &mut new_league3]
        /// ).await
        /// .ok();
        /// ```
        #vis async fn multi_insert_datasource<'a>(values: &'a mut [&'a mut #ty], datasource_name: &str) -> (
            Result<(), Box<dyn std::error::Error + Sync + std::marker::Send>> 
        ) {
            use crate::bounds::QueryParameters;
            
            let mut final_values: Vec<Vec<&dyn QueryParameters<'_>>> = Vec::new();
            for instance in values.iter() {
                let intermediate: &[&dyn QueryParameters<'_>] = &[#(#macro_fields_cloned),*];
                
                let mut longer_lived: Vec<&dyn QueryParameters<'_>> = Vec::new();
                for value in intermediate.into_iter() {
                    longer_lived.push(*value)
                }

                final_values.push(longer_lived)
            }
            
            let autogenerated_ids = <#ty as canyon_sql::canyon_crud::crud::CrudOperations<#ty>>::__insert_multi(
                #table_name,
                #pk,
                #column_names, 
                &mut final_values,
                datasource_name
            ).await;

            if let Err(error) = autogenerated_ids {
                Err(error)
            } else {
                for (idx, instance) in values.iter_mut().enumerate() {
                    instance.#pk_ident = autogenerated_ids
                        .as_ref()
                        .ok()
                        .unwrap()
                        .wrapper
                        .get(idx)
                        .expect("Failed getting the returned IDs for a multi insert")
                        .get::<&str, #pk_type>(#pk);
                }

                Ok(())
            }
        }
    }
}